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
> tracking sections go away. The design discussion is in [#2652].

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
- [ ] **M1: Exchange operators (core).**
  - [ ] `PipelinedRegistry`: executor-wide map from
        `(job, stage, task, partition)` to bounded output queues, with a
        bounded wait for producers that have not registered yet, and per-job
        cleanup.
  - [ ] `PipelinedShuffleWriterExec`: hash or passthrough routing into the
        registry, coalescing hash outputs back up to the batch size, an
        explicit end-of-stream marker per partition, and replayable broadcast
        outputs.
  - [ ] `PipelinedShuffleReaderExec`: connects when executed, drains every
        producer concurrently, no retries.
  - [ ] Protobuf messages, physical codec support, and the
        `FetchPipelinedPartition` Flight action.
  - [ ] Unit tests that run without sockets.
- [ ] **M2: Executor wiring.** Inject the registry into the execution engine
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

- M0 done: this page, the draft PR, and the `ballista.execution.mode`,
  `ballista.pipelined.queue_capacity` and
  `ballista.pipelined.registration_timeout_seconds` settings (documented in the
  user guide's config reference). Nothing reads them yet.

## Next steps

1. M1: the registry and the writer in
   `ballista/core/src/execution_plans/pipelined/`.
2. M1: the reader, the protobuf messages and the Flight action.

## Design decisions

Decisions made while implementing, with the reason, so they are not
relitigated. The design issue [#2652] records the overall architecture.

- **Push-staged only.** Standalone mode runs pull-staged, so tests use a
  push-staged standalone cluster (M5). Under pull-staged scheduling each
  executor claims work on its own poll, so a job cannot be placed atomically,
  and the executor's vcore permits would block pipelined tasks that wait on
  each other.

## How to test

Socket-based tests (anything that starts a scheduler, executor or gRPC
server) cannot run inside the Claude Code sandbox. Unit tests avoid sockets
where possible. The end-to-end tests run in CI on the draft PR.

```shell
cargo test --profile ci -p ballista-core pipelined
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
