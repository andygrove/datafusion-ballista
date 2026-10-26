<!---
  Licensed to the Apache Software Foundation (ASF) under one
  or more contributor license agreements.  See the NOTICE file
  distributed with this work for additional information
  regarding copyright ownership.  The ASF licenses this file
  to you under the Apache License, Version 2.0 (the
  "License"); you may not use this file except in compliance
  with the License.  You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

  Unless required by applicable law or agreed to in writing,
  software distributed under the License is distributed on an
  "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
  KIND, either express or implied.  See the License for the
  specific language governing permissions and limitations
  under the License.
-->

# Pipelined Execution (work in progress)

> **Status: work in progress.** This page tracks the plan, the current status
> and the next steps for the pipelined execution mode, so the work can be picked
> up from any machine. Once the mode lands it becomes the design page and the
> tracking sections go away. The design discussion is in [#2652], and the work
> happens on the draft PR [#2657] (branch `pipelined-execution` on
> `andygrove/datafusion-ballista`).

## Goal

An opt-in mode, `ballista.execution.mode = pipelined`, in which the scheduler
places every task of a job up front, and stages stream record batches to each
other over Arrow Flight instead of running stage by stage through shuffle
files. The blocking shuffle stays the default and is unchanged. See the
[shuffle design page](shuffle.md) for why both models exist.

## Plan

Each milestone is one or more commits on the draft PR. Tick items off as they
land and keep [Current status](#current-status) in step.

- [x] **M0: Scaffolding.** This page, the `ballista.execution.mode` setting,
      and the generated config docs.
- [x] **M1: Exchange operators (core).**
  - [x] `PipelinedRegistry`: executor-wide map from
        `(job, stage, task, partition)` to bounded output queues, with a
        bounded wait for producers that have not registered yet, and per-job
        cleanup.
  - [x] `PipelinedShuffleWriterExec`: hash or passthrough routing into the
        registry, coalescing hash outputs back up to the batch size, an
        explicit end-of-stream marker per partition, and replayable broadcast
        outputs.
  - [x] `PipelinedShuffleReaderExec`: connects when executed, drains every
        producer concurrently, no retries. Reads co-located producers straight
        from the registry, others over Flight.
  - [x] Protobuf messages, physical codec support, and the
        `FetchPipelinedPartition` Flight action.
  - [x] Unit tests that run without sockets.
- [x] **M2: Executor wiring.** Inject the registry into the execution engine
      and the Flight service, serve `FetchPipelinedPartition`, and clean up on
      `RemoveJobData`.
- [x] **M3: Scheduler.**
  - [x] `PipelinedExecutionGraph` built from `DefaultDistributedPlanner`.
  - [x] All-or-nothing placement in the push-staged bind pass, with
        per-executor reservations (the largest stage's vcores, not the sum).
  - [x] Resolve readers to producer locations at placement time.
  - [x] Publish the final stage's locations in the `Running` status.
  - [x] Fail the job on any task failure or lost executor, and cancel
        leftover tasks when the final stage completes.
  - [x] Reject pipelined jobs under pull-staged scheduling.
- [x] **M4: Client.** `DistributedQueryExec` reads the final stage while the
      job is running, reads all partitions concurrently, and surfaces a job
      failure from the status stream.
- [ ] **M5: Tests.**
  - [x] Standalone mode runs push-staged when the session asks for pipelined
        execution.
  - [x] End-to-end tests in `ballista/client/tests/pipelined.rs`: aggregates,
        joins, broadcast joins, unions, sorts, window functions, `LIMIT`, a
        failing query, remote reads, and pull-staged rejection.
  - [x] The end-to-end tests pass in CI.
  - [x] A multi-executor cluster, and a client that stops reading early.
  - [x] TPC-H SF10 in CI: a `pipelined` leg in `.github/workflows/tpch.yml`.
  - [x] The TPC-H leg passes.
  - [ ] Cancellation.
- [ ] **M6: Docs and polish.**
  - [x] User guide page: `docs/source/user-guide/pipelined-execution.md`.
  - [ ] Tuning notes and benchmarks against blocking mode.
  - [ ] Remove the tracking sections from this page.

## Current status

- **M0 done.** This page, the draft PR, and the `ballista.execution.mode`,
  `ballista.pipelined.queue_capacity` and
  `ballista.pipelined.registration_timeout_seconds` settings, documented in
  the user guide's config reference.
- **M1 done.** The exchange lives in
  `ballista/core/src/execution_plans/pipelined/`:
  - `registry.rs` holds the queues and broadcast replay buffers.
  - `writer.rs` drives every input partition and routes into the queues.
  - `reader.rs` streams every producer at once.
  - `tests.rs` runs writer against reader in-process.
  - New protobuf messages: `PipelinedShuffleWriterExecNode`,
    `PipelinedShuffleReaderExecNode`, `PipelinedPartitionLocation`,
    `FetchPipelinedPartition`, and `RunningJob.pipelined_output_locations`.
- **M2 done.**
  - `Executor` owns a `PipelinedRegistry` keyed by its executor id, and
    attaches it to every task's `SessionConfig` as an extension. That is how
    writers and readers find it, so plans need no rewriting.
  - The execution engine runs a `PipelinedShuffleWriterExec` root through
    `PipelinedQueryStageExec`.
  - `BallistaFlightService::with_pipelined_registry` serves
    `FetchPipelinedPartition` from `do_get`. Both the executor process and
    standalone executors wire it up.
  - `RemoveJobData` and pull-mode `jobs_to_clean` drop the job's registry
    entries.
- **M3 done.** The scheduler is in `ballista/scheduler/src/state/pipelined_graph.rs`.
  - **`PipelinedExecutionGraph`.** It plans the job with the static planner
    and swaps each stage root for a `PipelinedShuffleWriterExec`. A stage read
    through a broadcast reader is marked `broadcast`.
  - **Graph selection.** `TaskManager::submit_plan` picks this graph when
    `ballista.execution.mode = pipelined`, ignoring the AQE setting.
  - **Placement.** `plan_placement` is a pure function that spreads each
    stage's partitions round-robin over executors with free vcores, using
    contiguous ranges and capping tasks at `max_partitions_per_task`.
    - A collapse stage gets one task and one vcore.
    - The reservation on each executor is the largest number of vcores any
      single stage uses there.
    - The result is `Wait` if the job does not fit yet.
    - A stage wider than the cluster's total vcores, such as a `UNION ALL`
      of several scans, waits until every vcore is free and then runs
      several partitions per vcore. It used to fail the job.
  - **Binding.** `SchedulerState::bind_pipelined_jobs` runs before the normal
    binders in `revive_offers`.
    - It plans under the graph's read lock, reserves through the new
      `ClusterState::reserve_vcores` (all or nothing), and commits with
      `PipelinedExecutionGraph::place`, so the graph and vcore locks are never
      held together.
    - `place` resolves every `UnresolvedShuffleExec` to a
      `PipelinedShuffleReaderExec` and marks all stages running.
    - It also sets `RunningJob.pipelined_output_locations` and posts
      `JobUpdated`, which pushes the locations to the client.
  - **Releasing the reservation.** It is released once, when the job ends.
    `TaskManager::succeed_job` returns it and the `JobFinished` handler
    releases it; `abort_job` releases it on failure or cancellation. Every
    task's own `vcores_consumed` is 0 in scheduler bookkeeping.
  - **Failure.** A failed task fails the job (`JobRunningFailed`), and there
    are no retries. `TaskManager::executor_lost` reports pipelined jobs that
    used the lost executor; the handler then fails them.
  - **Completion.** When the final stage completes, every stage becomes
    `Successful`, the leftover running tasks are cancelled with `CancelTasks`,
    and `JobFinished` is posted.
  - **Pull-staged scheduling.** `SchedulerState::submit_job` rejects
    pipelined jobs under pull-staged.
  - **Per-task restriction.** `task_builder` restricts
    `PipelinedShuffleReaderExec` to each task's slice and leaves broadcast
    readers whole.
- **Tests.** Scheduler-level tests (`SchedulerTest`, virtual executors, no
  sockets) cover:
  - all stages launching before any task finishes;
  - success, task failure and cancellation, each releasing every vcore;
  - a job wider than the cluster running with several partitions per vcore;
  - a lost executor failing the job;
  - pull-staged rejection.
- **M4 done.** The client is in
  `ballista/core/src/execution_plans/distributed_query.rs`.
  - A `Running` status with non-empty `pipelined_output_locations` starts
    `pipelined_results`. It reads every partition of the final stage at
    once, with one `BallistaClient` per executor (or per proxy), and merges
    the streams with `select_all`. The final stage cannot finish until it
    has been read, so the partitions cannot be read one after another.
  - Partitions are fetched through `get_client_host_port`, so the scheduler
    or external Flight proxy is used when one is configured.
  - A watcher keeps following the job status: the rest of the pushed status
    stream in push mode, or polling every 50 ms in pull mode. If the job
    fails, the result stream ends with the job's error.
  - `SuccessfulJob.pipelined_output_locations` repeats the final stage's
    locations. A small result can finish before a polling client sees the
    job running, and the client can still read it, because the final stage
    keeps its queues until the delayed whole-job cleanup.
  - The registration timeout the client waits for each partition is the
    query's `ballista.pipelined.registration_timeout_seconds`.
- **M5 in progress.**

  - **Push-staged standalone.** `new_standalone_scheduler_with_builder` picks
    push-staged when the configuration asks for pipelined execution, and
    `new_standalone_executor_from_builder` then registers through
    `executor_server::startup`, with its task-launch gRPC server on a free
    port. Blocking sessions run pull-staged, as before.
  - **Scheduler endpoint.** The standalone scheduler now binds its listener
    before it is built, and advertises `localhost:<port>`. Push-staged
    executors report task status to the endpoint each launch names, and that
    used to be the hardcoded `localhost:50050`.
  - **End-to-end tests.** `ballista/client/tests/pipelined.rs` compares seven
    queries against blocking mode in three setups:

    - a standalone cluster;
    - the same with `ballista.shuffle.force_remote_read`;
    - a scheduler with two executors of two vcores each
      (`setup_test_cluster_with_executors` in `tests/common`), so every
      exchange crosses executors.

    It also checks:

    - a `LIMIT`;
    - a divide-by-zero reaching the client;
    - a pull-staged cluster rejecting a pipelined job;
    - a client that drops its results after one batch, after which a job
      needing every vcore still runs.

    The tests set `ballista.standalone.parallelism = 4`, so the
    four-partition plans fit whatever the machine's core count. They bind
    sockets, so they have only run in CI.

  - **First CI results** (commit `3f9a3c64`):

    - The client end-to-end tests passed on macOS. On Linux 7 of 8 passed.
      The one failure was the divide-by-zero test, which looked for
      "divide by zero" where the scheduler reports `ArrowError(DivideByZero)`.
      The assertion now accepts both.
    - The TPC-H SF10 `pipelined` leg matched DataFusion on q1 to q7, then
      failed q8: "Not enough memory to continue external sort". With sort-merge
      joins, all of the job's sorts (16 partitions in each of about 17
      stages) share the 2 GB pool at once, and a late sorter could not get
      even its small merge reservation. The leg now runs its executor with an
      8 GB pool. Pipelined jobs still keep to the executor's configured
      budget; the user guide explains the trade-off.

  - **CI green** (commit `f45b7a6b`). Every client end-to-end test passes on
    Linux and macOS. The TPC-H SF10 `pipelined` leg, with its 8 GB pool,
    matches DataFusion on all 21 queries. Its times, in seconds, against the
    blocking leg of the same run, one run each on shared runners: a rough
    first look, not a benchmark. The executor has 4 vcores and each stage 16
    partitions, so every pipelined stage is oversubscribed fourfold, and
    the queries use sort-merge joins, whose sorts all spill against one pool
    in pipelined mode.

    | Query | Blocking (AQE off) | Pipelined |
    | ----- | ------------------ | --------- |
    | 1     | 3.7                | 3.9       |
    | 2     | 0.8                | 3.4       |
    | 3     | 3.5                | 3.3       |
    | 4     | 1.5                | 2.1       |
    | 5     | 2.3                | 8.0       |
    | 6     | 1.0                | 0.9       |
    | 7     | 4.8                | 9.5       |
    | 8     | 2.2                | 9.3       |
    | 9     | 4.9                | 12.4      |
    | 10    | 3.5                | 2.7       |
    | 11    | 0.5                | 2.0       |
    | 12    | 2.5                | 2.5       |
    | 13    | 2.6                | 2.3       |
    | 14    | 1.2                | 1.1       |
    | 15    | 2.1                | 1.7       |
    | 17    | 6.3                | 9.6       |
    | 18    | 9.7                | 10.6      |
    | 19    | 3.2                | 2.2       |
    | 20    | 2.4                | 2.4       |
    | 21    | 8.7                | 18.5      |
    | 22    | 0.9                | 0.8       |

  - **TPC-H.** `.github/workflows/tpch.yml` gains a `pipelined` matrix leg.
    It runs every query but q16 on SF10 with `--verify` against DataFusion,
    on the workflow's one-executor cluster. That cluster runs push-staged,
    the binaries' default. Its executor has 4 vcores and queries use 16
    partitions, so every stage is wider than the cluster and shares vcores.
  - **One memory pool per job and executor.** With `--memory-pool-size`
    set, each blocking task gets a pool of `per_vcore × vcores_consumed`. A
    pipelined job's tasks all run at once, so pools sized task by task added
    up to more than the executor's memory. Now every task of the job carries
    the job's reservation on its executor as `vcores_consumed`. The executor
    gives all of the job's tasks there one shared runtime
    (`Executor::produce_runtime_for_pipelined_task`), and so one pool sized
    by that reservation.
  - **Launch failures.** When an executor rejects a pipelined task, or the
    launch call fails, the scheduler no longer refunds a vcore per task.
    Those vcores belong to the job's reservation, which the job's failure
    releases, so refunding them too credited the executor twice.
  - **Flight SQL.** The scheduler's Flight SQL backend returns a pipelined
    job's `QueryResult` as soon as the job is running with its final stage's
    locations, in the new `QueryResult.pipelined_partitions`. The frontend
    serves all of them through one endpoint, whose ticket (a new
    `StatementHandle::Pipelined`) names every partition and the schema.
    `DoGet` opens every partition through the Flight proxy at once and
    merges them. `ballista/scheduler/tests/flight_sql.rs` runs a pipelined
    cluster end to end.
  - **Review fixes.** A review of the whole branch found:
    - **Inputs read whole.** A cross or nested-loop join's left side, and
      anything else below a collapse in a stage of several tasks, is read
      in full by every task. Each such read opened a read-once queue, so a
      second task failed the job. Those producers are now replayed like a
      broadcast side (`task_builder::stages_read_whole`). The client tests
      gained a scalar-subquery query, which takes this path on the
      two-executor cluster.
    - **No reschedule after an abort.** The scheduling pass that follows a
      task report runs before the job failure that report causes, so a job
      waiting for a failed job's vcores was never placed. Now a failed or
      cancelled job's abort posts `ReviveOffers` under push-staged
      scheduling.
    - **Dictionaries over Flight.** Arrow Flight hydrates dictionary columns
      by default, so a remote reader got `Utf8` where the plan declared a
      dictionary. Both pipelined encoders now resend dictionaries.
    - **Late leftover tasks.** The registry now remembers removed stages as
      well as removed jobs. A leftover task that starts after its stage was
      cleaned up fails at once instead of waiting out its timeouts.
    - **Placement errors.** An error partway through a placement pass no
      longer drops the tasks of the jobs it had already placed.
  - **Client hardening.**
    - A result stream dropped before its end cancels the job, which frees
      its vcores at once. Before, the job ran on until its producers
      noticed: at once for partitions the client had opened, and only after
      the claim timeout for any it had not.
    - A failed fetch waits up to 5 seconds for the job's own failure and
      reports that instead. A failed job's data is cleaned up from its
      executors straight away, so a fetch that arrives late reports only
      that the job is gone.
  - **Unclaimed queues time out.** A producer waits on a full queue that no
    consumer has opened only for `ballista.pipelined.registration_timeout_seconds`,
    then fails its task, which fails the job. Before, a client that
    disconnected before opening its results left the final stage blocked
    and the job's vcores reserved forever.

## Next steps

1. **Keep CI green.** On [#2657]
   (`gh pr checks 2657 -R apache/datafusion-ballista`), the pipelined
   coverage is the "test linux ballista" and "macos test" jobs (client
   end-to-end tests) and the "TPC-H SF10 (pipelined)" job, which uploads the
   cluster logs when it fails. If 8 GB becomes too little for a query, lower
   that leg's partitions rather than raise the pool further: the runner has
   16 GB, and the benchmark's DataFusion check shares it. Reading the logs
   from a Claude Code session needs GitHub's Actions storage hosts
   (`productionresultssa*.blob.core.windows.net`) on the sandbox allowlist.
2. **Cancellation.** Scheduler tests cover a cancelled job releasing its
   vcores and a waiting job being placed after it. The client's
   cancel-on-drop is exercised end to end by
   `dropping_the_results_early_frees_the_cluster`. Nothing yet cancels a
   running job through the scheduler API end to end and checks that its
   registry entries are gone.
3. **M6: benchmarks and tuning.** The first numbers above oversubscribe every
   stage fourfold on a single executor. A fair comparison runs pipelined
   with as many partitions as the cluster has vcores, on more than one
   executor. Then add tuning notes for `ballista.pipelined.queue_capacity`
   and executor memory to the user guide page.

## Design decisions

Decisions made while implementing, with the reason, so they are not
relitigated. The design issue [#2652] records the overall architecture.

- **Push-staged only.** Under pull-staged scheduling each executor claims
  work on its own poll, so a job cannot be placed atomically, and the
  executor's vcore permits would block pipelined tasks that wait on each
  other. Standalone mode therefore runs push-staged when the session asks for
  pipelined execution.
- **Bounded queues, not unbounded channels.**
  - Each non-broadcast output partition is a bounded `mpsc` queue (capacity
    `ballista.pipelined.queue_capacity`, in batches), so a slow consumer
    stalls its producer.
  - DataFusion Distributed lets data for a slow consumer pile up in the
    producer instead. Bounded queues are deadlock-free for tree-shaped plans
    as long as every reader drains all of its producers concurrently, which
    `PipelinedShuffleReaderExec` does.
  - The hash writer finishes partitions concurrently, so one full queue does
    not hold back the end markers of the others.
- **Explicit end-of-stream marker.** A queue that closes without
  `Message::End` belongs to a producer that failed or was cancelled. The
  consumer gets an error carrying the producer's failure, never a silently
  truncated partition.
- **A producer task finishes when its input does.**
  - It does not wait for consumers to drain, so its vcores are released
    promptly.
  - Undrained queues stay in the registry until their consumer claims them or
    the job is cleaned up.
  - If every consumer goes away (for example a satisfied `LIMIT`), the writer
    stops reading its input.
- **Broadcast outputs are replayed.** Every consumer task reads each broadcast
  partition in full, so those outputs are retained, unbounded, until job
  cleanup. Their size is bounded by
  `ballista.optimizer.broadcast_join_threshold_bytes`.
- **So is any output read whole by a stage of several tasks.** A cross or
  nested-loop join's left side is the usual case. No threshold bounds these,
  but blocking mode reads the same data once per task too. A collapse stage
  runs as one task, so it reads through queues.
- **The registry reaches tasks through `SessionConfig`.** Tasks find the
  executor's registry as a session config extension rather than through plan
  rewriting in the execution engine, so custom engines that leave the plan
  alone still work.
- **Co-located reads skip Flight.** When the producer runs on the reader's
  executor (matched by executor id), the reader opens the registry directly.
  `ballista.shuffle.force_remote_read` forces the Flight path, which
  multi-executor tests can use.
- **The registry remembers finished jobs.** It keeps the last 1024 cleaned-up
  job ids. A producer task that starts after its job was cleaned up fails
  instead of waiting forever for consumers that are gone.
- **A stage wider than the cluster shares vcores rather than failing.** The
  reservation is already a soft budget: stages share it, and a waiting task
  is a parked future, not a thread. A `UNION ALL` is as wide as all its
  inputs together, so refusing such stages would fail ordinary queries. The
  cost is that a wide job waits for the whole cluster to be free.
- **A job's tasks on an executor share one memory pool.** The pool is sized
  by the job's reservation there, as a blocking task's pool is sized by its
  own vcores. The tasks run at once, so one shared pool bounds the job's
  memory there, and per-task pools would not. The executor holds each job's
  runtime weakly, so it goes away when the job's last task there finishes.
- **The reservation is held for the whole job.** A pipelined job's vcores are
  reserved once, at placement, and released once, when it ends. Releasing per
  task would hand vcores back while other tasks of the job still run on the
  same executor. The job's task infos carry `vcores_consumed = 0`, so the
  normal per-task refunds stay correct. The `TaskDescription` sent to the
  executor carries the job's reservation there, which sizes the memory pool
  the job's tasks there share.
- **Placement and reservation never hold two locks.** The placement is planned
  under the graph read lock, the vcores are reserved atomically, and the
  placement is committed under the graph write lock. That avoids the
  vcore-lock-then-graph-lock order that `bind_schedulable_tasks` uses.
- **The client reads every final partition at once.** A pipelined job's
  final stage cannot finish until its output is read, so the client cannot
  wait for `SuccessfulJob` the way blocking mode does, or read partitions one
  after another.
- **One timeout bounds both sides of an exchange.** A consumer waits up to
  `ballista.pipelined.registration_timeout_seconds` for its producer to
  register. A producer waits the same time for a consumer to open a full
  queue. It does not wait on a slow consumer that has opened its queue;
  that is ordinary backpressure. The producer waits on a reservation in the
  queue rather than a send, so timing out loses no batch.
- **Flight SQL serves a pipelined result through one endpoint.** A client
  may read endpoints one at a time, and a pipelined job cannot finish
  writing one partition until its other partitions are being read. Merging
  on the scheduler, which every Flight SQL result already passes through,
  keeps any client from stalling the job.
- **The fetch responds before the producer exists.** The Flight `do_get` for a
  pipelined partition returns its response at once and waits for the producer
  inside the stream body. A consumer that arrives early is therefore not cut
  off by its client's request timeout. The consumer's registration timeout
  travels in the ticket.

## How to test

Socket-based tests (anything that starts a scheduler, executor or gRPC
server) cannot run inside the Claude Code sandbox. Unit tests avoid sockets
where possible. The end-to-end tests run in CI on the draft PR.

```shell
cargo test --profile ci -p ballista-core --lib pipelined
cargo test --profile ci -p ballista-executor --lib flight_service
cargo test --profile ci -p ballista-scheduler pipelined
# End to end; needs sockets
cargo test --profile ci -p ballista --test pipelined
```

## Open questions

These are carried over from [#2652]:

1. Is `ballista.execution.mode = blocking | pipelined` the right name and set
   of values?
2. Should a pipelined job reserve the largest stage's vcores on each executor,
   or whole executors?
3. Should a pipelined job that does not fit right now wait, or fall back to
   blocking execution?
4. Should producers start eagerly, or only when a consumer first polls them?

[#2652]: https://github.com/apache/datafusion-ballista/issues/2652
[#2657]: https://github.com/apache/datafusion-ballista/pull/2657
