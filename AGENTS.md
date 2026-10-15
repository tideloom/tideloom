# AGENTS.md

Single source of truth for AI coding agents and new contributors in the
Tideloom repository. Read this before touching code. The design rationale
lives in `docs/`; this file is the operating manual.

## Language

English in code, comments, identifiers, commit messages, and PR text. The
ADR and the milestone documents under `docs/` are written in Chinese; keep
their language when editing them.

## Project snapshot

Tideloom is a Rust runtime for the Serverless Workflow DSL 1.0.

- Toolchain: stable Rust, `rust-version = 1.90`, edition 2024. CI also runs
  nightly `clippy` and `rustfmt` (the `rustfmt.toml` uses unstable options,
  so format with `cargo +nightly fmt`).
- Execution model: an immutable definition tree is interpreted by a pure
  function; the resume state is an explicit, serializable `Continuation`.
  Nothing is replayed from the root. See ADR-0001.
- Two crates:
  - `tideloom-core`: definitions, continuation, interpreter, expressions.
    The HTTP client still lives here until M3 MR-H4 moves it. No tokio, no
    clock.
  - `tideloom-runtime`: everything with side effects. Store, wake-up bus,
    executors, definition source, events, clock, and the `Engine` that
    composes them. Traits first, implementations in their own PRs.

## Documentation map

- `docs/ADR-0001-serverless-workflow-execution-model.md`: why the model is
  what it is. Status Accepted. Amend it with a dated revision line when a
  decision changes; never rewrite history.
- `docs/milestone-m2-single-process-runtime.md`: the completed single-process
  milestone. Historical; do not reopen it.
- `docs/milestone-m3-postgres-and-two-workers.md`: the current milestone,
  split into PRs with boundaries, dependencies, and the files each one owns.
  This is the work queue.
- `docs/runtime-architecture.md`: how the runtime traits fit together, the
  engine step, and the invariants each trait implementation must keep.
- `README.md`: user-facing description of the core API.

## Repository layout

```text
tideloom/
├── Cargo.toml                 # virtual workspace; no root crate
├── tideloom-core/
│   ├── src/definition.rs      # YAML/JSON -> immutable Node tree, content hash
│   ├── src/node.rs            # Node, NodeKind, the three predicates
│   ├── src/continuation.rs    # Continuation, frames, Resume, SCHEMA
│   ├── src/key.rs             # ExecutionKey, Counter, TaskKey
│   ├── src/spec.rs            # BlockDetail: wait, branches, child, listen
│   ├── src/walk.rs            # the interpreter: start, start_at, resume
│   ├── src/expr.rs            # the expression subset
│   ├── src/retry.rs           # backoff, jitter, durations, Timestamp
│   ├── src/fault.rs           # RFC 7807 faults
│   ├── src/http.rs            # call: http; M3 MR-H4 moves this out of core
│   └── tests/                 # one file per concern, see below
├── tideloom-runtime/
│   ├── src/instance.rs        # Instance, InstanceId, Status, WaitReason
│   ├── src/commit.rs          # Commit, StepResult, SpawnChild
│   ├── src/store.rs           # InstanceStore trait
│   ├── src/wakeup.rs          # Wakeup trait, Signal, Offset
│   ├── src/executor.rs        # ActivityExecutor trait, TaskKind
│   ├── src/definitions.rs     # DefinitionSource trait + MemoryDefinitions
│   ├── src/events.rs          # CloudEvent, EventSink, EventSource
│   ├── src/clock.rs           # Clock, ManualClock, SystemClock
│   ├── src/engine.rs          # Engine, EngineParts, Engine::commit
│   └── tests/contract.rs      # the contracts later PRs rely on
└── docs/
```

## Non-negotiable rules

1. `tideloom-core` stays pure. No tokio, no I/O, no `SystemTime`, no trait
   objects. It takes a definition, a continuation, and an event, and returns
   an `Outcome`. If the core needs a new capability, extend one of exactly
   four types: `Pause`, `Block` (via `BlockDetail`), `Resume`, `Outcome`.
   Say so in the PR under a heading "Core API changes".
2. `Continuation` is a persistence format. `SCHEMA` in `continuation.rs` is
   its version. Any change to a frame's shape bumps `SCHEMA` and ships a
   read path from the previous version, or the PR explains why rejecting old
   continuations is acceptable. Say so in the PR under "Persistence format
   changes".
3. The journal (`task_execution`, `StepResult`) is written, never read back
   into a run. The continuation is the only resume state. Do not add a code
   path that reconstructs state from step results.
4. `InstanceStore::apply` and `Wakeup::commit_offset` are called only from
   `Engine::commit`, in that order. Do not call either anywhere else.
5. Signals on the bus are hints. Correctness must hold with every signal
   lost; the sweeper and `wake_at` are the backstop. A test that drops all
   signals must still pass.
6. The engine never sleeps and never reads the wall clock directly. Time
   comes from the `Clock` trait. Tests use `ManualClock`.
7. Executors run outside the store transaction. Before an executor runs,
   a `Pending` step row is committed; after it, the outcome. A still-`Pending`
   row on recovery is re-run only when `ActivityExecutor::idempotent` is true.
8. Faults are RFC 7807 values from `Fault::*`. Runner contract violations
   (wrong `Resume` variant, unknown executor) fault the instance directly and
   are not catchable by `try`.
9. No `unwrap`/`expect` in library code. `todo!`/`unimplemented!` never
   reach `main`.

## Working on a milestone PR

The milestone document lists the PRs. For each one:

1. Check its dependency row. Do not start a PR whose dependency is not
   merged unless the row says it can run in parallel.
2. Branch from `main` as `m3/<number>-<short-name>`. Title the PR
   `M3 MR-<number>: <short name>`. M2 used `m2/` and `M2 MR-`; that queue
   is closed.
3. Touch only the files the ownership table assigns to that PR. If you need
   a trait or a core type changed, open a separate, tiny PR for that change
   first and say which MR needs it.
4. Add the test file the milestone names. Tests inject the clock and never
   sleep.
5. The PR description has three mandatory sections, even if each says "none":
   "Core API changes", "Persistence format changes", and "Trait signature
   changes".
6. Run the full check before pushing (below). CI runs the same.

## Build and test commands

```bash
cargo build --workspace
cargo test --workspace
cargo test -p tideloom-core                    # must not depend on the runtime crate
cargo test -p tideloom-runtime --test contract
cargo clippy --workspace --all-targets         # warnings are failures in review
cargo +nightly fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Capture long test runs to a file and grep it rather than re-running:

```bash
cargo test --workspace 2>&1 | tee /tmp/tideloom-<topic>.log
grep -E "test result|FAILED|panicked" /tmp/tideloom-<topic>.log
```

## Test layout

- `tideloom-core/tests/build.rs`: definition building, predicates, hashes.
- `tideloom-core/tests/walk.rs`: inline semantics, start/resume, keys.
- `tideloom-core/tests/spec.rs`: `BlockDetail`, `start_at`, schema checks.
- `tideloom-core/tests/retry.rs`, `retry_limit.rs`, `timeout.rs`: policies
  and deadlines, driven by `Resume::Tick` with an injected `now`.
- `tideloom-runtime/tests/http_call.rs`: `call: http` against a local TCP
  fixture; no mocks of the client. The client still lives in
  `tideloom-core/src/http.rs` until M3 MR-H4 moves it.
- `tideloom-runtime/tests/contract.rs`: trait object safety, helpers,
  task routing. Milestone PRs add `memory_store.rs`, `local_wakeup.rs`,
  `engine.rs`, `journal.rs`, `fork.rs`, `child_workflow.rs`, `listen.rs`.

Test naming reads as a sentence: `a_jump_back_through_an_effectful_task_gets_a_new_pass_key`.

## Key types to know

- `Definition` / `Node`: the immutable tree. Positions are JSON Pointers;
  the root pointer is the empty string. `composite`, `blocking`, `effectful`
  are computed at build time.
- `Continuation`: frames (`Seq`, `For`, `Try`, `Function`) plus the pending
  block, `$context`, workflow input, and the recorded starts. `root_task()`
  is `Some` when the run was started with `start_at` (a `fork` branch).
- `Block`: what a runner receives. `key()` is position plus `ExecutionKey`
  (`loop:n`, `attempt:n`, `pass:n`). `detail()` is the pause-specific data.
- `Resume`: `Output`, `Fault`, `Release`, `Tick`.
- `Instance`, `Commit`: the store's unit and its atomic change.
- `TaskKind`: executor routing, derived from `NodeKind` by `TaskKind::of`.

## Things that look wrong but are deliberate

- `Outcome::Blocked` carries the whole `Continuation` by value. One per
  block; the clippy size lint is allowed on purpose.
- `wait: {}` is a zero wait, not an error, so tests can pause without a
  duration.
- `listen` filters are static strings only. Expressions, `correlate`, and
  `until` fault at block creation with a configuration fault; they are
  milestone work, not bugs.
- `InstanceId::new` reads the wall clock through `Uuid::now_v7`. Ids carry
  no deadline, so they do not go through `Clock`. Event ids are document or
  executor values, likewise not clock readings.
- Events are not retained for a subscription that does not exist yet. The
  M2 `LocalEventBus` replays a retained log into a new inbox; M3 MR-H6
  removes that replay. A lost event before `subscribe` is lost.
