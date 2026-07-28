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

//! End-to-end join correctness suite.
//!
//! Every case is checked differentially: the same SQL over the same parquet
//! files is run on a plain single-node DataFusion context, which has no shuffle,
//! no stage splitting and no adaptive planning, and the two result sets must
//! match. That makes the oracle genuinely independent of the machinery under
//! test.
//!
//! The suite matters because a join's physical strategy in Ballista is decided
//! by three code paths that have to agree:
//!
//! * `ballista/scheduler/src/physical_optimizer/join_selection.rs` (static)
//! * `ballista/scheduler/src/planner.rs` (broadcast demotion, stage splitting)
//! * `ballista/scheduler/src/state/aqe/execution_plan/dynamic_join.rs` (AQE)
//!
//! Null-aware anti joins (`NOT IN (subquery)`) are the sharpest case: they track
//! probe-side state that is only correct in `CollectLeft` mode, so all three
//! paths carry a special case refusing to demote or swap them. When one of those
//! guards regresses the query still returns rows, just the wrong ones, which is
//! exactly the failure mode a differential test catches and an eyeball does not.

mod join_fixtures;

#[cfg(test)]
#[cfg(feature = "standalone")]
mod join_tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use ballista::prelude::{SessionConfigExt, SessionContextExt};
    use ballista_core::config::{
        BALLISTA_ADAPTIVE_PLANNER_ENABLED, BALLISTA_BROADCAST_JOIN_THRESHOLD_BYTES,
        BALLISTA_BROADCAST_JOIN_THRESHOLD_ROWS,
        BALLISTA_HASH_JOIN_MAX_BUILD_PARTITION_BYTES,
        BALLISTA_SCHEDULER_MAX_PARTITIONS_PER_TASK, BALLISTA_STANDALONE_PARALLELISM,
    };
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
    use datafusion::arrow::util::pretty::pretty_format_batches;
    use datafusion::common::Result;
    use datafusion::execution::SessionStateBuilder;
    use datafusion::prelude::{SessionConfig, SessionContext};
    use rstest::rstest;
    use tokio::sync::OnceCell;

    use crate::join_fixtures::Fixtures;

    // -----------------------------------------------------------------------
    // Configuration axes
    // -----------------------------------------------------------------------

    /// Which physical join strategy the configuration forces.
    ///
    /// Forcing is necessary. `SessionConfig::new_with_ballista()` sets
    /// `prefer_hash_join = false` and broadcast thresholds of 10 MiB / 1M rows,
    /// and the fixtures are small, so left alone every case in the suite would
    /// broadcast and the shuffle paths would never run.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Strategy {
        /// Build side collected onto one partition and broadcast.
        Broadcast,
        /// Both sides hash-repartitioned on the join key.
        PartitionedHash,
        /// Both sides hash-repartitioned, joined with a sort-merge join.
        SortMerge,
    }

    impl Strategy {
        fn label(&self) -> &'static str {
            match self {
                Strategy::Broadcast => "broadcast",
                Strategy::PartitionedHash => "partitioned_hash",
                Strategy::SortMerge => "sort_merge",
            }
        }
    }

    /// Pairs a partition count with the scheduler's per-task partition cap.
    ///
    /// The cap is the axis that actually shapes tasks: `bind_one`
    /// (cluster/mod.rs:432) slices a stage's pending partitions into tasks of
    /// at most this many. A full cross-product against `target_partitions`
    /// would waste most of the matrix, because with a single target partition
    /// there is nothing to slice and every cap value behaves identically.
    /// These pairs spend the configurations on the cap instead.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Slicing {
        /// One partition, so no slicing is possible. The degenerate case where
        /// the union and empty-stage bugs (#2185, #2186) surfaced.
        SinglePartition,
        /// Eight partitions capped at one per task: the maximum number of
        /// tasks, and the most shuffle metadata to get right.
        OnePerTask,
        /// Eight partitions, two per task.
        TwoPerTask,
        /// Eight partitions, four per task.
        FourPerTask,
        /// Eight partitions, cap disabled: a single task takes all of them.
        Unbounded,
        /// A non-power-of-two partition count, which exercises uneven slicing
        /// (three partitions capped at two gives tasks of 2 and 1).
        OddPartitions,
    }

    impl Slicing {
        fn target_partitions(&self) -> usize {
            match self {
                Slicing::SinglePartition => 1,
                Slicing::OddPartitions => 3,
                _ => 8,
            }
        }

        /// `0` means the cap is disabled.
        fn max_partitions_per_task(&self) -> usize {
            match self {
                Slicing::SinglePartition => 0,
                Slicing::OnePerTask => 1,
                Slicing::TwoPerTask => 2,
                Slicing::FourPerTask => 4,
                Slicing::Unbounded => 0,
                Slicing::OddPartitions => 2,
            }
        }
    }

    /// One point in the configuration matrix.
    #[derive(Debug, Clone, Copy)]
    struct Config {
        aqe: bool,
        strategy: Strategy,
        slicing: Slicing,
    }

    impl Config {
        fn target_partitions(&self) -> usize {
            self.slicing.target_partitions()
        }

        fn max_partitions_per_task(&self) -> usize {
            self.slicing.max_partitions_per_task()
        }
    }

    impl std::fmt::Display for Config {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "aqe={} strategy={} partitions={} max_per_task={}",
                self.aqe,
                self.strategy.label(),
                self.target_partitions(),
                match self.max_partitions_per_task() {
                    0 => "unbounded".to_string(),
                    n => n.to_string(),
                },
            )
        }
    }

    /// Builds a standalone Ballista cluster configured for `config`.
    async fn cluster(config: Config) -> SessionContext {
        // A large value that no fixture can exceed, used to force broadcasts.
        const HUGE: &str = "1073741824";

        let mut session_config = SessionConfig::new_with_ballista()
            .with_target_partitions(config.target_partitions())
            .set_bool(BALLISTA_ADAPTIVE_PLANNER_ENABLED, config.aqe)
            .set_str(
                BALLISTA_SCHEDULER_MAX_PARTITIONS_PER_TASK,
                &config.max_partitions_per_task().to_string(),
            )
            // Keep the thread footprint bounded: the matrix runs many of these
            // clusters concurrently under cargo's test parallelism.
            .set_str(BALLISTA_STANDALONE_PARALLELISM, "4");

        session_config = match config.strategy {
            Strategy::Broadcast => session_config
                .set_str(BALLISTA_BROADCAST_JOIN_THRESHOLD_BYTES, HUGE)
                .set_str(BALLISTA_BROADCAST_JOIN_THRESHOLD_ROWS, HUGE)
                .set_u64(
                    "datafusion.optimizer.hash_join_single_partition_threshold",
                    1024 * 1024 * 1024,
                )
                .set_u64(
                    "datafusion.optimizer.hash_join_single_partition_threshold_rows",
                    1024 * 1024 * 1024,
                ),
            // Thresholds of 0 disable broadcast promotion on both the Ballista
            // and the DataFusion side, so both inputs must be repartitioned.
            Strategy::PartitionedHash => session_config
                .set_str(BALLISTA_BROADCAST_JOIN_THRESHOLD_BYTES, "0")
                .set_str(BALLISTA_BROADCAST_JOIN_THRESHOLD_ROWS, "0")
                .set_u64(
                    "datafusion.optimizer.hash_join_single_partition_threshold",
                    0,
                )
                .set_u64(
                    "datafusion.optimizer.hash_join_single_partition_threshold_rows",
                    0,
                )
                .set_bool("datafusion.optimizer.prefer_hash_join", true)
                // AQE does not consult `prefer_hash_join`; it falls back to a
                // sort-merge join when a build partition exceeds this. 0
                // disables that fallback, keeping the join a hash join.
                .set_str(BALLISTA_HASH_JOIN_MAX_BUILD_PARTITION_BYTES, "0"),
            Strategy::SortMerge => session_config
                .set_str(BALLISTA_BROADCAST_JOIN_THRESHOLD_BYTES, "0")
                .set_str(BALLISTA_BROADCAST_JOIN_THRESHOLD_ROWS, "0")
                .set_u64(
                    "datafusion.optimizer.hash_join_single_partition_threshold",
                    0,
                )
                .set_u64(
                    "datafusion.optimizer.hash_join_single_partition_threshold_rows",
                    0,
                )
                .set_bool("datafusion.optimizer.prefer_hash_join", false)
                // Mirror of the above: a 1-byte cap makes AQE always prefer the
                // spillable sort-merge join.
                .set_str(BALLISTA_HASH_JOIN_MAX_BUILD_PARTITION_BYTES, "1"),
        };

        let state = SessionStateBuilder::new()
            .with_config(session_config)
            .with_default_features()
            .build();

        SessionContext::standalone_with_state(state)
            .await
            .expect("standalone cluster starts")
    }

    // -----------------------------------------------------------------------
    // Query catalogue
    // -----------------------------------------------------------------------

    /// Whether the plan-shape guard applies to a case.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Shape {
        /// A plain inner equi-join, whose plan must show the configured
        /// strategy. Only inner joins qualify: `collect_left_broadcast_safe`
        /// (join_selection.rs:128) admits only `Inner`/`Right*`, so a `LEFT` or
        /// `FULL` join is demoted to `Partitioned` even under the broadcast
        /// configuration and would make the guard fire spuriously.
        Strategy,
        /// Exempt from the guard. Covers the outer/semi/anti joins demoted as
        /// described above, null-aware anti joins (required to stay
        /// `CollectLeft` everywhere), set operations, nested-loop joins, and
        /// empty-input cases that the optimizer may collapse to an `EmptyExec`
        /// with no join left in the plan at all.
        Exempt,
    }

    /// How a case linked to an open bug should be handled.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Behavior {
        /// Do not execute. Reserved for bugs whose symptom is a hang: a hang
        /// cannot be caught and would otherwise stall the whole suite.
        Skip,
        /// Execute and require the result to be wrong. If it starts matching
        /// the oracle the test fails, telling us to drop the marker, so a fix
        /// cannot silently leave stale annotations behind.
        Xfail,
    }

    /// Links a case to an open bug, for the configurations where it bites.
    struct KnownIssue {
        /// Issue number in apache/datafusion-ballista.
        issue: u32,
        /// What goes wrong, in one line.
        symptom: &'static str,
        behavior: Behavior,
        /// The configurations the bug applies to. Bugs here are
        /// strategy-dependent, so a blanket marker would hide the
        /// configurations that do work.
        applies: fn(&Config) -> bool,
    }

    struct Case {
        name: &'static str,
        sql: &'static str,
        shape: Shape,
        /// Open bugs affecting this case. A case can carry more than one,
        /// because the same query fails differently depending on the strategy:
        /// `NOT IN` returns wrong rows under sort-merge (#2193) but hangs under
        /// a partitioned hash join (#2187).
        known_issues: Vec<KnownIssue>,
    }

    impl Case {
        /// The first open bug affecting this case under `config`, if any.
        fn issue_for(&self, config: &Config) -> Option<&KnownIssue> {
            self.known_issues
                .iter()
                .find(|issue| (issue.applies)(config))
        }

        /// Records that this case is blocked on an open bug for the
        /// configurations matched by `applies`.
        fn tracked(
            mut self,
            issue: u32,
            symptom: &'static str,
            behavior: Behavior,
            applies: fn(&Config) -> bool,
        ) -> Self {
            self.known_issues.push(KnownIssue {
                issue,
                symptom,
                behavior,
                applies,
            });
            self
        }
    }

    fn case(name: &'static str, sql: &'static str) -> Case {
        Case {
            name,
            sql,
            shape: Shape::Strategy,
            known_issues: Vec::new(),
        }
    }

    fn exempt(name: &'static str, sql: &'static str) -> Case {
        Case {
            name,
            sql,
            shape: Shape::Exempt,
            known_issues: Vec::new(),
        }
    }

    /// Wall-clock cap on a single case, overridable with `JOIN_CASE_TIMEOUT`
    /// (seconds). Generous by default relative to how long these queries
    /// actually take, so tripping it means the query is stuck rather than slow.
    fn case_timeout() -> Duration {
        std::env::var("JOIN_CASE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(90))
    }

    /// The only strategy that sets `prefer_hash_join = true`, so the only one
    /// where a `NOT IN` keeps its null-aware `HashJoinExec` and reaches the
    /// swap/repartition path of #2187. Under the other strategies the physical
    /// planner has already replaced it with a `SortMergeJoinExec` (#2193).
    fn is_partitioned_hash(config: &Config) -> bool {
        config.strategy == Strategy::PartitionedHash
    }

    /// The catalogue, optionally narrowed by the `JOIN_CASES` environment
    /// variable to case names containing a substring. Useful when
    /// investigating one failure across the matrix without waiting for the
    /// other forty cases.
    fn select_cases() -> Vec<Case> {
        match std::env::var("JOIN_CASES") {
            Ok(filter) if !filter.trim().is_empty() => cases()
                .into_iter()
                .filter(|c| c.name.contains(filter.trim()))
                .collect(),
            _ => cases(),
        }
    }

    /// The full catalogue. Adding a case is one line; it is automatically
    /// covered by every configuration in the matrix.
    fn cases() -> Vec<Case> {
        vec![
            // ---- regular joins, every join type -------------------------
            case("inner", "SELECT l.k, l.v, r.w FROM l JOIN r ON l.k = r.k"),
            exempt(
                "left_outer",
                "SELECT l.k, l.v, r.w FROM l LEFT JOIN r ON l.k = r.k",
            ),
            exempt(
                "right_outer",
                "SELECT l.k, l.v, r.w FROM l RIGHT JOIN r ON l.k = r.k",
            ),
            exempt(
                "full_outer",
                "SELECT l.k, l.v, r.k, r.w FROM l FULL OUTER JOIN r ON l.k = r.k",
            ),
            exempt(
                "left_semi",
                "SELECT l.k, l.v FROM l LEFT SEMI JOIN r ON l.k = r.k",
            ),
            exempt(
                "right_semi",
                "SELECT r.k, r.w FROM l RIGHT SEMI JOIN r ON l.k = r.k",
            ),
            exempt(
                "left_anti",
                "SELECT l.k, l.v FROM l LEFT ANTI JOIN r ON l.k = r.k",
            ),
            exempt(
                "right_anti",
                "SELECT r.k, r.w FROM l RIGHT ANTI JOIN r ON l.k = r.k",
            ),
            // ---- join shape variations ----------------------------------
            case(
                "inner_with_filter",
                "SELECT l.k, l.v, r.w FROM l JOIN r ON l.k = r.k AND r.w > 500",
            ),
            case(
                "inner_composite_key",
                "SELECT a.k1, a.k2, count(*) AS n \
                 FROM comp a JOIN comp b ON a.k1 = b.k1 AND a.k2 = b.k2 \
                 GROUP BY a.k1, a.k2",
            ),
            exempt(
                "left_outer_composite_key",
                "SELECT a.k1, a.k2, count(b.v) AS n \
                 FROM comp a LEFT JOIN comp b ON a.k1 = b.k1 AND a.k2 = b.k2 \
                 GROUP BY a.k1, a.k2",
            ),
            case(
                "inner_skewed_key",
                "SELECT l.k, count(*) AS n FROM l JOIN skew s ON l.k = s.k GROUP BY l.k",
            ),
            case(
                "join_then_aggregate",
                "SELECT r.k, count(*) AS n, sum(l.v) AS sv \
                 FROM l JOIN r ON l.k = r.k GROUP BY r.k",
            ),
            case(
                "three_way_join",
                "SELECT l.k, count(*) AS n \
                 FROM l JOIN r ON l.k = r.k JOIN r_nonull n2 ON l.k = n2.k \
                 GROUP BY l.k",
            ),
            // ---- empty inputs -------------------------------------------
            exempt(
                "inner_empty_build",
                "SELECT l.k, l.v, e.w FROM l JOIN empty e ON l.k = e.k",
            ),
            exempt(
                "left_outer_empty_build",
                "SELECT l.k, l.v, e.w FROM l LEFT JOIN empty e ON l.k = e.k",
            ),
            exempt(
                "inner_empty_probe",
                "SELECT e.k, r.w FROM empty e JOIN r ON e.k = r.k",
            ),
            exempt(
                "full_outer_empty_build",
                "SELECT l.k, e.k FROM l FULL OUTER JOIN empty e ON l.k = e.k",
            ),
            // ---- non-equi and cross joins -------------------------------
            // Both sides are pre-filtered so the nested-loop product stays
            // small; the join itself is still a genuine nested-loop join.
            exempt(
                "cross_join",
                "SELECT a.k, b.k FROM (SELECT * FROM l WHERE v < 10) a \
                 CROSS JOIN (SELECT * FROM r WHERE w < 10) b",
            ),
            exempt(
                "non_equi_inner",
                "SELECT a.k, b.k FROM (SELECT * FROM l WHERE v < 20) a \
                 JOIN (SELECT * FROM r WHERE w < 20) b ON a.k < b.k",
            ),
            exempt(
                "non_equi_left_outer",
                "SELECT a.k, b.k FROM (SELECT * FROM l WHERE v < 20) a \
                 LEFT JOIN (SELECT * FROM r WHERE w < 20) b ON a.k < b.k",
            ),
            // ---- mark join ----------------------------------------------
            // `EXISTS` inside a disjunction is the only way to reach a mark
            // join; it has no surface SQL syntax.
            exempt(
                "mark_join_exists_or",
                "SELECT l.k, l.v FROM l \
                 WHERE EXISTS (SELECT 1 FROM r WHERE r.k = l.k) OR l.v < 10",
            ),
            // ---- semi/anti via subquery ---------------------------------
            exempt(
                "in_subquery",
                "SELECT l.k, l.v FROM l WHERE l.k IN (SELECT k FROM r)",
            ),
            exempt(
                "exists_subquery",
                "SELECT l.k, l.v FROM l WHERE EXISTS (SELECT 1 FROM r WHERE r.k = l.k)",
            ),
            // ---- null-aware anti joins ----------------------------------
            // These produce `HashJoinExec { null_aware: true }`, which must stay
            // in `CollectLeft` mode in every configuration. Exempt from the
            // shape guard for exactly that reason.
            //
            // `r.k` contains NULLs, so SQL `NOT IN` semantics make the whole
            // result empty. Getting rows back here means null-awareness was lost.
            exempt(
                "not_in_build_has_nulls",
                "SELECT l.k, l.v FROM l WHERE l.k NOT IN (SELECT k FROM r)",
            )
            // Partitioned hash keeps `prefer_hash_join = true`, so the join
            // stays a null-aware `HashJoinExec` and hits the swap/repartition
            // path. The symptom is the hang #2187 describes: stage resolution
            // fails and the distributed query runs until the client gives up.
            // Skipped rather than run, because a hang cannot be asserted on.
            .tracked(
                2187,
                "null-aware anti join hangs when repartitioned",
                Behavior::Skip,
                is_partitioned_hash,
            )
            // Every other strategy inherits Ballista's `prefer_hash_join =
            // false`, so the physical planner turns this into a
            // `SortMergeJoinExec`, which carries no `null_aware` flag. The
            // NULL in the subquery is ignored and rows come back that SQL
            // semantics say must not: observed 582 rows against an expected 0.
            .tracked(
                2193,
                "SortMergeJoinExec drops null_aware, so NOT IN returns rows",
                Behavior::Xfail,
                |_| true,
            ),
            // The control: no NULLs on the build side, so rows must come back.
            // Paired with the case above, this distinguishes "null-awareness
            // works" from "the query returns nothing for some other reason".
            //
            // Still null-sensitive despite the NULL-free build side, because
            // `l.k` itself contains NULLs and `NULL NOT IN (...)` is UNKNOWN,
            // so those left rows must be dropped. An anti join that has lost
            // its null-awareness emits them.
            exempt(
                "not_in_build_no_nulls",
                "SELECT l.k, l.v FROM l WHERE l.k NOT IN (SELECT k FROM r_nonull)",
            )
            .tracked(
                2187,
                "null-aware anti join hangs when repartitioned",
                Behavior::Skip,
                is_partitioned_hash,
            )
            .tracked(
                2193,
                "SortMergeJoinExec drops null_aware, so NULL-keyed left rows survive",
                Behavior::Xfail,
                |_| true,
            ),
            // Empty build side: every non-NULL left row qualifies. Correct
            // under the non-hash strategies, because null-awareness does not
            // change the answer when the subquery has no rows at all.
            exempt(
                "not_in_build_empty",
                "SELECT l.k, l.v FROM l WHERE l.k NOT IN (SELECT k FROM empty)",
            )
            .tracked(
                2187,
                "null-aware anti join hangs when repartitioned",
                Behavior::Skip,
                is_partitioned_hash,
            ),
            // Build side is entirely NULL: result must be empty. Also correct
            // under the non-hash strategies.
            exempt(
                "not_in_build_all_null",
                "SELECT l.k, l.v FROM l WHERE l.k NOT IN (SELECT k FROM all_null)",
            )
            .tracked(
                2187,
                "null-aware anti join hangs when repartitioned",
                Behavior::Skip,
                is_partitioned_hash,
            ),
            // `NOT EXISTS` is *not* null-aware. Pairing it with the `NOT IN`
            // cases above proves the two are not being conflated.
            exempt(
                "not_exists_correlated",
                "SELECT l.k, l.v FROM l \
                 WHERE NOT EXISTS (SELECT 1 FROM r WHERE r.k = l.k)",
            ),
            // ---- null-equality joins ------------------------------------
            // `INTERSECT`/`EXCEPT` plan joins with
            // `NullEquality::NullEqualsNull`, a flag that has to survive the
            // protobuf roundtrip (see ballista/core/src/serde/mod.rs:1517).
            // If it decodes as `NullEqualsNothing` the NULL rows silently
            // disappear from the result.
            exempt("intersect", "SELECT k FROM l INTERSECT SELECT k FROM r"),
            exempt(
                "intersect_all",
                "SELECT k FROM l INTERSECT ALL SELECT k FROM r",
            ),
            exempt("except", "SELECT k FROM l EXCEPT SELECT k FROM r"),
            exempt("except_all", "SELECT k FROM l EXCEPT ALL SELECT k FROM r"),
            // An explicit null-equality join condition: NULL matches NULL.
            // Aggregated so the result stays small while the join still runs in
            // full.
            exempt(
                "is_not_distinct_from",
                "SELECT l.k, count(*) AS n FROM l \
                 JOIN r ON l.k IS NOT DISTINCT FROM r.k GROUP BY l.k",
            ),
        ]
    }

    // -----------------------------------------------------------------------
    // Fixtures and oracle, built once per test process
    // -----------------------------------------------------------------------

    static FIXTURES: OnceCell<Fixtures> = OnceCell::const_new();
    static ORACLE: OnceCell<HashMap<&'static str, Vec<String>>> = OnceCell::const_new();

    async fn fixtures() -> &'static Fixtures {
        FIXTURES
            .get_or_init(|| async { Fixtures::generate().expect("fixtures generate") })
            .await
    }

    /// Expected results, computed once for the whole process by a plain
    /// single-node DataFusion context.
    ///
    /// Computing these per configuration instead would run the oracle 54 times
    /// per query and dominate the suite's runtime for no extra coverage.
    async fn oracle() -> &'static HashMap<&'static str, Vec<String>> {
        ORACLE
            .get_or_init(|| async {
                let ctx = SessionContext::new_with_config(
                    SessionConfig::new().with_target_partitions(4),
                );
                fixtures()
                    .await
                    .register(&ctx)
                    .await
                    .expect("register fixtures on oracle context");

                let mut expected = HashMap::new();
                for c in cases() {
                    let batches = ctx
                        .sql(c.sql)
                        .await
                        .unwrap_or_else(|e| {
                            panic!(
                                "oracle failed to plan case '{}': {e}\n{}",
                                c.name, c.sql
                            )
                        })
                        .collect()
                        .await
                        .unwrap_or_else(|e| {
                            panic!(
                                "oracle failed to run case '{}': {e}\n{}",
                                c.name, c.sql
                            )
                        });
                    expected.insert(c.name, normalize(&batches));
                }
                expected
            })
            .await
    }

    // -----------------------------------------------------------------------
    // Result comparison
    // -----------------------------------------------------------------------

    /// Renders batches to one string per row and sorts them.
    ///
    /// Sorting the rows rather than adding `ORDER BY` to the SQL keeps the plan
    /// under test unchanged, and sidesteps NULL-ordering differences entirely.
    fn normalize(batches: &[RecordBatch]) -> Vec<String> {
        let options = FormatOptions::default().with_null("NULL");
        let mut rows = Vec::new();

        for batch in batches {
            if batch.num_rows() == 0 {
                continue;
            }
            let formatters: Vec<ArrayFormatter> = batch
                .columns()
                .iter()
                .map(|c| {
                    ArrayFormatter::try_new(c.as_ref(), &options)
                        .expect("array is formattable")
                })
                .collect();

            for row in 0..batch.num_rows() {
                let rendered: Vec<String> = formatters
                    .iter()
                    .map(|f| f.value(row).to_string())
                    .collect();
                rows.push(rendered.join(" | "));
            }
        }

        rows.sort();
        rows
    }

    /// Compares one case against the oracle, with a failure message that names
    /// the configuration. With 54 configurations, a bare `assert_eq!` on
    /// batches would be undebuggable.
    fn describe_mismatch(
        config: &Config,
        case: &Case,
        expected: &[String],
        actual: &[String],
    ) -> Option<String> {
        if expected == actual {
            return None;
        }

        let missing: Vec<&String> =
            expected.iter().filter(|r| !actual.contains(r)).collect();
        let extra: Vec<&String> =
            actual.iter().filter(|r| !expected.contains(r)).collect();

        Some(format!(
            "case '{}' returned the wrong rows\n\
             \x20 config:   {}\n\
             \x20 sql:      {}\n\
             \x20 expected: {} rows, actual: {} rows\n\
             \x20 missing from actual ({} rows, first 5): {:?}\n\
             \x20 unexpected in actual ({} rows, first 5): {:?}",
            case.name,
            config,
            case.sql,
            expected.len(),
            actual.len(),
            missing.len(),
            missing.iter().take(5).collect::<Vec<_>>(),
            extra.len(),
            extra.iter().take(5).collect::<Vec<_>>(),
        ))
    }

    // -----------------------------------------------------------------------
    // Plan-shape guard
    // -----------------------------------------------------------------------

    /// Asserts that the configured strategy is actually in effect.
    ///
    /// Without this the matrix can go green while testing nothing: if every
    /// fixture were small enough to broadcast, the partitioned-hash
    /// configuration would silently degenerate into the broadcast one and
    /// report success 18 times over.
    ///
    /// Only checked with the adaptive planner off. Under AQE the join operator
    /// is chosen at runtime from observed statistics, so `EXPLAIN` shows the
    /// pre-adaptive plan and cannot confirm the final choice.
    async fn assert_strategy_in_effect(
        ctx: &SessionContext,
        config: &Config,
        cases: &[Case],
    ) -> Result<()> {
        if config.aqe {
            return Ok(());
        }

        // With a single target partition there is nothing to repartition, so
        // DataFusion collapses every strategy to the same single-partition
        // plan (observed: `mode=CollectLeft` even with broadcast thresholds at
        // 0 and `prefer_hash_join = true`). That is correct planning, not a
        // collapsed axis, so the guard does not apply. The cases themselves
        // still run and are still checked against the oracle.
        if config.target_partitions() == 1 {
            return Ok(());
        }

        for case in cases.iter().filter(|c| c.shape == Shape::Strategy) {
            let plan = ctx
                .sql(&format!("EXPLAIN {}", case.sql))
                .await?
                .collect()
                .await?;
            let text = pretty_format_batches(&plan)?.to_string();

            let collapsed = |want: &str| {
                format!(
                    "expected {want} for case '{}' under {config}; the strategy axis \
                     has collapsed, so this configuration is testing nothing.\n\
                     sql: {}\nplan:\n{text}",
                    case.name, case.sql,
                )
            };

            match config.strategy {
                Strategy::Broadcast => {
                    assert!(
                        text.contains("mode=CollectLeft"),
                        "{}",
                        collapsed("a broadcast (CollectLeft) hash join")
                    );
                    assert!(
                        !text.contains("mode=Partitioned"),
                        "{}",
                        collapsed("no partitioned hash join")
                    );
                }
                Strategy::PartitionedHash => {
                    assert!(
                        text.contains("mode=Partitioned"),
                        "{}",
                        collapsed("a partitioned hash join")
                    );
                    assert!(
                        !text.contains("mode=CollectLeft"),
                        "{}",
                        collapsed("no broadcast join")
                    );
                    assert!(
                        !text.contains("SortMergeJoinExec"),
                        "{}",
                        collapsed("no sort-merge join")
                    );
                }
                Strategy::SortMerge => {
                    assert!(
                        text.contains("SortMergeJoinExec"),
                        "{}",
                        collapsed("a sort-merge join")
                    );
                    assert!(
                        !text.contains("HashJoinExec"),
                        "{}",
                        collapsed("no hash join")
                    );
                }
            }

            // A distributed join must actually redistribute its inputs. If
            // everything collapsed into one stage there is no shuffle left to
            // get wrong.
            if config.strategy != Strategy::Broadcast && config.target_partitions() > 1 {
                assert!(
                    text.contains("ShuffleWriterExec")
                        || text.contains("RepartitionExec"),
                    "expected the inputs of case '{}' to be redistributed under \
                     {config}, got:\n{text}",
                    case.name,
                );
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // The matrix
    // -----------------------------------------------------------------------

    /// Runs the whole query catalogue against one cluster configuration.
    ///
    /// `rstest` expands this into one `#[test]` per combination, so cargo
    /// parallelises across configurations while each configuration pays for
    /// cluster startup exactly once.
    #[rstest]
    #[tokio::test]
    async fn join_matrix(
        #[values(false, true)] aqe: bool,
        #[values(Strategy::Broadcast, Strategy::PartitionedHash, Strategy::SortMerge)]
        strategy: Strategy,
        #[values(
            Slicing::SinglePartition,
            Slicing::OnePerTask,
            Slicing::TwoPerTask,
            Slicing::FourPerTask,
            Slicing::Unbounded,
            Slicing::OddPartitions
        )]
        slicing: Slicing,
    ) -> Result<()> {
        let config = Config {
            aqe,
            strategy,
            slicing,
        };

        let expected_all = oracle().await;
        let ctx = cluster(config).await;
        fixtures().await.register(&ctx).await?;

        let cases = select_cases();
        assert_strategy_in_effect(&ctx, &config, &cases).await?;

        // Every case runs even after one fails, and failures are reported
        // together at the end. One config exercises forty-odd cases; aborting
        // at the first failure hides the rest and turns diagnosis into a
        // sequence of one-bug-at-a-time reruns.
        let mut failures: Vec<String> = Vec::new();

        // `JOIN_IGNORE_MARKERS=1` runs every case as if it were unmarked. Use
        // it to see the true state of the suite when validating a fix, without
        // editing the catalogue.
        let ignore_markers = std::env::var("JOIN_IGNORE_MARKERS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        for case in cases {
            let tracked = if ignore_markers {
                None
            } else {
                case.issue_for(&config)
            };

            if let Some(issue) = tracked
                && issue.behavior == Behavior::Skip
            {
                eprintln!(
                    "SKIP  {} under {config}: {} (issue #{})",
                    case.name, issue.symptom, issue.issue,
                );
                continue;
            }

            let expected = expected_all
                .get(case.name)
                .unwrap_or_else(|| panic!("no oracle entry for case '{}'", case.name));

            let outcome = run_case(&ctx, &case, &config).await;

            match tracked {
                // Marked as blocked on an open bug, so it is expected to be
                // wrong. Producing the right answer means the bug is fixed and
                // the marker is now hiding real coverage, which is itself a
                // failure.
                Some(issue) => {
                    if let Outcome::Ok(actual) = &outcome
                        && actual == expected
                    {
                        failures.push(format!(
                            "case '{}' is marked as blocked on issue #{} ({}) but \
                             now produces the correct result. If #{} is fixed, \
                             remove the `.tracked(...)` marker so the suite \
                             enforces this case again.",
                            case.name, issue.issue, issue.symptom, issue.issue,
                        ));
                        continue;
                    }
                    eprintln!(
                        "XFAIL {} under {config}: {} (issue #{})",
                        case.name, issue.symptom, issue.issue,
                    );
                }
                None => match outcome {
                    Outcome::Ok(actual) => {
                        if let Some(why) =
                            describe_mismatch(&config, &case, expected, &actual)
                        {
                            failures.push(why);
                        }
                    }
                    Outcome::Failed(why) => {
                        failures.push(format!("case '{}': {why}", case.name));
                    }
                },
            }
        }

        assert!(
            failures.is_empty(),
            "{} of the join cases failed under {config}:\n\n{}",
            failures.len(),
            failures.join("\n\n"),
        );

        Ok(())
    }

    /// What happened when a case ran.
    enum Outcome {
        Ok(Vec<String>),
        Failed(String),
    }

    /// Runs one case under a wall-clock cap.
    ///
    /// The cap is what makes an unknown hang actionable. Ballista can leave a
    /// distributed query running indefinitely when stage resolution fails (see
    /// issue #2187), and without a bound that stalls the whole suite instead of
    /// reporting a failure.
    async fn run_case(ctx: &SessionContext, case: &Case, config: &Config) -> Outcome {
        let attempt = async {
            let df = ctx.sql(case.sql).await?;
            let batches = df.collect().await?;
            Ok::<_, datafusion::error::DataFusionError>(normalize(&batches))
        };

        let limit = case_timeout();
        match tokio::time::timeout(limit, attempt).await {
            Ok(Ok(rows)) => Outcome::Ok(rows),
            Ok(Err(e)) => Outcome::Failed(format!("query error: {e}")),
            Err(_) => Outcome::Failed(format!(
                "timed out after {}s under {config}; the query never completed",
                limit.as_secs(),
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Standalone assertions about the fixtures themselves
    // -----------------------------------------------------------------------

    /// The null-aware cases are only meaningful if the fixtures have the
    /// properties they assume. If `r` ever loses its NULL keys, or `l` and
    /// `r_nonull` stop overlapping, the `NOT IN` cases would pass vacuously.
    #[tokio::test]
    async fn fixtures_have_the_properties_the_null_aware_cases_assume() -> Result<()> {
        let ctx = SessionContext::new_with_config(
            SessionConfig::new().with_target_partitions(4),
        );
        fixtures().await.register(&ctx).await?;

        let count = |batches: &[RecordBatch]| -> i64 {
            use datafusion::arrow::array::Int64Array;
            batches[0]
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("count(*) is Int64")
                .value(0)
        };

        let r_nulls = ctx
            .sql("SELECT count(*) FROM r WHERE k IS NULL")
            .await?
            .collect()
            .await?;
        assert!(
            count(&r_nulls) > 0,
            "`r` must contain NULL keys, otherwise not_in_build_has_nulls passes vacuously"
        );

        let r_nonull_nulls = ctx
            .sql("SELECT count(*) FROM r_nonull WHERE k IS NULL")
            .await?
            .collect()
            .await?;
        assert_eq!(
            count(&r_nonull_nulls),
            0,
            "`r_nonull` must not contain NULL keys; it is the control for the null-aware cases"
        );

        // `NOT IN` against a NULL-containing build side must be empty, and
        // against the null-free control must not be. This is the semantic
        // difference the whole null-aware axis exists to protect.
        let with_nulls = ctx
            .sql("SELECT count(*) FROM l WHERE l.k NOT IN (SELECT k FROM r)")
            .await?
            .collect()
            .await?;
        assert_eq!(
            count(&with_nulls),
            0,
            "NOT IN against a NULL-containing subquery must return no rows"
        );

        let without_nulls = ctx
            .sql("SELECT count(*) FROM l WHERE l.k NOT IN (SELECT k FROM r_nonull)")
            .await?
            .collect()
            .await?;
        assert!(
            count(&without_nulls) > 0,
            "NOT IN against a NULL-free subquery must return rows, \
             otherwise not_in_build_no_nulls proves nothing"
        );

        // Outer joins need genuinely unmatched rows on both sides to be
        // meaningful.
        let unmatched_left = ctx
            .sql("SELECT count(*) FROM l LEFT ANTI JOIN r ON l.k = r.k")
            .await?
            .collect()
            .await?;
        assert!(
            count(&unmatched_left) > 0,
            "`l` must have rows with no match in `r`, otherwise the outer joins \
             degenerate into inner joins"
        );

        let unmatched_right = ctx
            .sql("SELECT count(*) FROM l RIGHT ANTI JOIN r ON l.k = r.k")
            .await?
            .collect()
            .await?;
        assert!(
            count(&unmatched_right) > 0,
            "`r` must have rows with no match in `l`, otherwise RIGHT/FULL OUTER \
             degenerate into inner joins"
        );

        Ok(())
    }
}
