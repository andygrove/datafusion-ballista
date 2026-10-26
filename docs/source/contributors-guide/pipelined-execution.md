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
- [ ] **M3: Scheduler.**
  - [ ] `PipelinedExecutionGraph` built from `DefaultDistributedPlanner`.
  - [ ] All-or-nothing placement in the push-staged bind pass, with
        per-executor reservations (the largest stage's vcores, not the sum).
  - [ ] Resolve readers to producer locations at placement time.
  - [ ] Publish the final stage's locations in the `Running` status.
  - [ ] Fail the job on any task failure or lost executor, and cancel
        leftover tasks when the final stage completes.
  - [ ] Reject pipelined jobs under pull-staged scheduling.
- [ ] **M4: Client.** `DistributedQueryExec` reads the final stage while the
      job is running, reads all partitions concurrently, and surfaces a job
      failure from the status stream.
- [ ] **M5: Tests.** A push-staged standalone cluster for tests, and end-to-end
      tests: TPC-H correctness, joins, broadcast joins, unions, sort-preserving
      merges, `LIMIT`, cancellation and task failure.
- [ ] **M6: Docs and polish.** User guide page, tuning notes, benchmarks
      against blocking mode, and removal of the tracking sections.

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
    Nothing sets the last one yet.
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
- Nothing creates pipelined plans yet, so the setting still has no effect.
  That is M3.

## Next steps

1. **M3: `PipelinedExecutionGraph`** in
   `ballista/scheduler/src/state/pipelined_graph.rs`.
   - Plan the stages with `DefaultDistributedPlanner`.
   - Swap each stage root for a `PipelinedShuffleWriterExec`: a hash
     `SortShuffleWriterExec` becomes hash, a `ShuffleWriterExec` becomes
     passthrough, and a stage read through a broadcast `UnresolvedShuffleExec`
     sets `broadcast`.
   - Select the graph in `TaskManager::submit_plan` when
     `ballista_config().execution_mode()` is `Pipelined`.
2. **M3: all-or-nothing placement.** Add a pre-pass in
   `InMemoryClusterState::bind_schedulable_tasks`, before the distribution
   policy runs, that places every task of a pipelined job at once.
   - Resolve each consumer's `UnresolvedShuffleExec` to a
     `PipelinedShuffleReaderExec` from the placement.
   - Teach `task_builder::restrict_plan_to_partitions` about the new reader;
     `PipelinedShuffleReaderExec::with_partitions` is ready for it.
3. **M3: status, failure and pull-staged handling.**
   - Push the final stage's locations in `RunningJob`.
   - Fail the job on any task failure or lost executor.
   - Cancel leftover tasks once the final stage succeeds.
   - Reject the job under pull-staged scheduling.

## Design decisions

Decisions made while implementing, with the reason, so they are not
relitigated. The design issue [#2652] records the overall architecture.

- **Push-staged only.** Standalone mode runs pull-staged, so tests use a
  push-staged standalone cluster (M5). Under pull-staged scheduling each
  executor claims work on its own poll, so a job cannot be placed atomically,
  and the executor's vcore permits would block pipelined tasks that wait on
  each other.
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
