// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Bounded connection pool for `BallistaClient` instances.
//!
//! `DefaultBallistaClientPool` keeps, per `(host, port, config)` endpoint, up to
//! [`MAX_CONNECTIONS_PER_ENDPOINT`] reusable connections guarded by a semaphore.
//! `acquire` checks out a connection **exclusively** — reusing an idle one, or
//! opening a new one, and waiting if all permits are in use — and the returned
//! `PooledClient` returns the connection to the idle set and releases the permit
//! when dropped.
//!
//! Each connection serves one request at a time, so this reuses a small bounded
//! set of connections. That avoids both failure modes seen at high
//! `target_partitions`: the per-fetch connection **churn** of opening a
//! connection per request (which raced connection teardown against in-flight
//! reads — broken pipe — and exhausted ephemeral ports), and **multiplexing**
//! many streams over one connection (which deadlocks on the shared h2
//! flow-control window under heavy shuffle load).
//!
//! `PooledClient::discard` drops the connection instead of returning it (still
//! releasing the permit), for error handling. A background task evicts idle
//! connections unused within `idle_timeout`.

use ballista_core::client::BallistaClient;
use ballista_core::client_pool::{BallistaClientPool, PooledClient};
use ballista_core::error::Result;
use ballista_core::extension::BallistaConfigGrpcEndpoint;
use ballista_core::utils::GrpcClientConfig;
use dashmap::DashMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Maximum number of connections held (and concurrently checked out) per endpoint.
const MAX_CONNECTIONS_PER_ENDPOINT: usize = 64;

struct IdleEntry {
    client: BallistaClient,
    idle_since: Instant,
}

/// Per-endpoint state: a bounded set of reusable connections.
struct Endpoint {
    /// Returned, ready-to-reuse connections.
    idle: Mutex<Vec<IdleEntry>>,
    /// Bounds the number of concurrently checked-out connections.
    permits: Arc<Semaphore>,
}

impl Endpoint {
    fn new() -> Self {
        Self {
            idle: Mutex::new(Vec::new()),
            permits: Arc::new(Semaphore::new(MAX_CONNECTIONS_PER_ENDPOINT)),
        }
    }
}

type EndpointMap = DashMap<(String, u16, GrpcClientConfig), Arc<Endpoint>>;

struct Inner {
    endpoints: EndpointMap,
    idle_timeout: Duration,
}

/// Default pool implementation.
///
/// Keeps up to `MAX_CONNECTIONS_PER_ENDPOINT` reusable connections per
/// `(host, port, config)`, handed out exclusively via a semaphore. Idle
/// connections are evicted by a background tokio task that runs at
/// `idle_timeout / 3` intervals (minimum 15 s). The task exits automatically
/// when the pool `Arc` is dropped.
#[derive(Clone)]
pub struct DefaultBallistaClientPool {
    inner: Arc<Inner>,
}

impl Debug for DefaultBallistaClientPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefaultBallistaClientPool").finish()
    }
}

impl DefaultBallistaClientPool {
    /// Create a pool that evicts connections idle longer than `idle_timeout`.
    pub fn with_eviction_thread(idle_timeout: Duration) -> Self {
        Self::new(idle_timeout, true)
    }

    /// Create a pool that evicts connections idle longer than `idle_timeout`,
    /// if `enable_eviction_thread` is enabled.
    pub fn new(idle_timeout: Duration, enable_eviction_thread: bool) -> Self {
        let inner = Arc::new(Inner {
            endpoints: DashMap::new(),
            idle_timeout,
        });

        let weak: Weak<Inner> = Arc::downgrade(&inner);
        // there is no empirical evidence why 15 is selected.
        // we can revisit if interval < 15 is needed
        let check_interval = Duration::from_secs((idle_timeout.as_secs() / 3).max(15));

        if enable_eviction_thread {
            tokio::spawn(async move {
                log::debug!(
                    "client connection pool - eviction thread started ... interval: {check_interval:?}"
                );
                let mut ticker = tokio::time::interval(check_interval);
                loop {
                    ticker.tick().await;

                    match weak.upgrade() {
                        None => break,
                        Some(pool) => {
                            log::trace!("client connection pool - evicting connections");
                            evict(&pool.endpoints, pool.idle_timeout)
                        }
                    }
                }
                log::debug!("client connection pool - eviction thread ... DONE");
            });
        }

        Self { inner }
    }

    #[cfg(test)]
    /// Total number of idle connections currently held across all endpoints.
    pub fn idle_count(&self) -> usize {
        self.inner
            .endpoints
            .iter()
            .map(|e| e.value().idle.lock().unwrap().len())
            .sum()
    }
}

fn evict(endpoints: &EndpointMap, timeout: Duration) {
    let deadline = Instant::now()
        .checked_sub(timeout)
        .unwrap_or_else(Instant::now);

    // Drop idle connections not reused within the timeout so the pool shrinks
    // under low utilization. Keep the endpoint entry while it still has idle
    // connections or connections checked out.
    endpoints.retain(|_, ep| {
        let mut idle = ep.idle.lock().unwrap();
        idle.retain(|e| e.idle_since >= deadline);
        let in_use = ep.permits.available_permits() < MAX_CONNECTIONS_PER_ENDPOINT;
        !idle.is_empty() || in_use
    });
}

#[async_trait::async_trait]
impl BallistaClientPool for DefaultBallistaClientPool {
    async fn acquire(
        &self,
        host: &str,
        port: u16,
        config: &GrpcClientConfig,
        customize_endpoint: Option<Arc<BallistaConfigGrpcEndpoint>>,
    ) -> Result<PooledClient> {
        let key = (host.to_string(), port, config.clone());
        let endpoint = self
            .inner
            .endpoints
            .entry(key)
            .or_insert_with(|| Arc::new(Endpoint::new()))
            .clone();

        // Wait for a connection slot; bounds concurrent connections per endpoint.
        let permit = endpoint
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("client pool semaphore is never closed");

        // Reuse an idle connection if one is available, else open a new one.
        let idle = endpoint.idle.lock().unwrap().pop();
        let client = match idle {
            Some(entry) => {
                log::trace!(
                    "client connection pool - reusing connection - host:{host}, port:{port}"
                );
                entry.client
            }
            None => {
                log::trace!(
                    "client connection pool - opening NEW connection - host:{host}, port:{port}"
                );
                BallistaClient::try_new(
                    host,
                    port,
                    config.max_message_size,
                    config.use_tls,
                    customize_endpoint,
                    config.io_retries_times,
                    config.io_retry_wait_time_ms,
                )
                .await?
            }
        };

        // On drop: return the connection to the idle set, then release the slot
        // (the captured `permit`). `discard()` drops this closure instead, which
        // releases the slot without returning the connection.
        let endpoint_ret = endpoint.clone();
        Ok(PooledClient::new(
            client,
            Box::new(move |c| {
                endpoint_ret.idle.lock().unwrap().push(IdleEntry {
                    client: c,
                    idle_since: Instant::now(),
                });
                drop(permit);
            }),
        ))
    }

    async fn evict_idle(&self) {
        evict(&self.inner.endpoints, self.inner.idle_timeout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ballista_core::client::BallistaClient;
    use std::time::Duration;

    fn make_pool(timeout: Duration) -> DefaultBallistaClientPool {
        DefaultBallistaClientPool::new(timeout, false)
    }

    /// Inject an idle connection with a specific `idle_since` directly into the
    /// pool, bypassing `acquire` so no real server is needed.
    fn inject_idle(
        pool: &DefaultBallistaClientPool,
        host: &str,
        port: u16,
        age: Duration,
    ) {
        let endpoint = pool
            .inner
            .endpoints
            .entry((host.to_string(), port, GrpcClientConfig::default()))
            .or_insert_with(|| Arc::new(Endpoint::new()))
            .clone();
        endpoint.idle.lock().unwrap().push(IdleEntry {
            client: BallistaClient::new_for_test(host, port),
            idle_since: Instant::now() - age,
        });
    }

    #[tokio::test]
    async fn idle_count_starts_at_zero() {
        let pool = make_pool(Duration::from_secs(60));
        assert_eq!(pool.idle_count(), 0);
    }

    #[tokio::test]
    async fn evict_idle_does_not_panic_on_empty_pool() {
        let pool = make_pool(Duration::from_secs(60));
        pool.evict_idle().await;
        assert_eq!(pool.idle_count(), 0);
    }

    /// An idle connection older than `idle_timeout` must be removed by `evict_idle`.
    #[tokio::test]
    async fn evict_idle_removes_expired_entries() {
        let timeout = Duration::from_millis(100);
        let pool = make_pool(timeout);

        inject_idle(&pool, "host-a", 1234, timeout + Duration::from_millis(50));
        assert_eq!(pool.idle_count(), 1);

        pool.evict_idle().await;
        assert_eq!(pool.idle_count(), 0);
    }

    /// A connection younger than `idle_timeout` must survive eviction.
    #[tokio::test]
    async fn evict_idle_keeps_fresh_entries() {
        let timeout = Duration::from_secs(60);
        let pool = make_pool(timeout);

        inject_idle(&pool, "host-b", 2345, Duration::from_millis(10));
        assert_eq!(pool.idle_count(), 1);

        pool.evict_idle().await;
        assert_eq!(pool.idle_count(), 1);
    }

    /// Dropping a [PooledClient] must return the connection to the pool.
    #[tokio::test]
    async fn pooled_client_returns_on_drop() {
        let pool = make_pool(Duration::from_secs(300));
        let key = ("host-c".to_string(), 3456u16, GrpcClientConfig::default());
        let endpoint = pool
            .inner
            .endpoints
            .entry(key)
            .or_insert_with(|| Arc::new(Endpoint::new()))
            .clone();
        let permit = endpoint.permits.clone().acquire_owned().await.unwrap();

        let endpoint_ret = endpoint.clone();
        let guard = PooledClient::new(
            BallistaClient::new_for_test("host-c", 3456),
            Box::new(move |c| {
                endpoint_ret.idle.lock().unwrap().push(IdleEntry {
                    client: c,
                    idle_since: Instant::now(),
                });
                drop(permit);
            }),
        );

        assert_eq!(pool.idle_count(), 0);
        drop(guard);
        assert_eq!(pool.idle_count(), 1);
    }

    /// Calling `discard()` must drop the connection instead of returning it.
    #[tokio::test]
    async fn discard_does_not_return_to_pool() {
        let pool = make_pool(Duration::from_secs(300));
        let key = ("host-d".to_string(), 4567u16, GrpcClientConfig::default());
        let endpoint = pool
            .inner
            .endpoints
            .entry(key)
            .or_insert_with(|| Arc::new(Endpoint::new()))
            .clone();
        let permit = endpoint.permits.clone().acquire_owned().await.unwrap();

        let endpoint_ret = endpoint.clone();
        let guard = PooledClient::new(
            BallistaClient::new_for_test("host-d", 4567),
            Box::new(move |c| {
                endpoint_ret.idle.lock().unwrap().push(IdleEntry {
                    client: c,
                    idle_since: Instant::now(),
                });
                drop(permit);
            }),
        );

        guard.discard();
        assert_eq!(pool.idle_count(), 0);
    }

    /// Mixed scenario: one expired and one fresh endpoint — only the expired one
    /// is removed, the other survives.
    #[tokio::test]
    async fn evict_idle_partial_removal() {
        let timeout = Duration::from_millis(100);
        let pool = make_pool(timeout);

        inject_idle(&pool, "host-e", 5678, timeout + Duration::from_millis(50)); // stale
        inject_idle(&pool, "host-f", 6789, Duration::from_millis(10)); // fresh
        assert_eq!(pool.idle_count(), 2);

        pool.evict_idle().await;
        assert_eq!(pool.idle_count(), 1);
    }
}
