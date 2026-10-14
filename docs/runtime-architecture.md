# Runtime architecture

How `tideloom-runtime` turns the pure interpreter in `tideloom-core` into a
system that runs workflows. ADR-0001 says why; this document says how, at
the level of traits and invariants, so that each implementation can be built
and reviewed on its own.

## The split

```text
             tideloom-core (pure)                 tideloom-runtime (effects)
┌─────────────────────────────────────┐   ┌──────────────────────────────────────┐
│ Definition  ── start / start_at ──► │   │  Engine                               │
│             ── resume(event) ─────► │◄──│   claim ─ decode ─ run ─ Commit ─ apply│
│ Continuation ◄── Outcome::Blocked   │   │                                       │
│ Block { key, pause, detail }        │   │  InstanceStore   Wakeup   Clock       │
│ Resume { Output Fault Release Tick }│   │  ActivityExecutor  DefinitionSource   │
└─────────────────────────────────────┘   │  EventSink / EventSource              │
                                          └──────────────────────────────────────┘
```

The core knows nothing about instances, workers, or time. It is called with
a definition, a continuation, an event, and an optional clock reading, and
answers with one of three outcomes. Every side effect is the engine's.

## Types the engine moves around

| Type | Lives in | Role |
|---|---|---|
| `Instance` | store | One run: definition key, parent, status, serialized continuation, version, lease, `wake_at`, input, output, fault |
| `Commit` | engine → store | Everything one step changes, applied atomically under a CAS on `version` |
| `StepResult` | engine → store | One journal row for an effectful task: `Pending`, `Output`, `Fault`, or `Unknown` |
| `SpawnChild` | engine → store | A child instance created in the same commit as its parent's change |
| `Signal` | engine ↔ bus | A hint that an instance may be runnable, with a log offset |
| `TaskExecution` | engine → executor | One effectful task with its input, `$context`, and `$workflow.input` |
| `CloudEvent` | executor → sink, source → engine | Events for `emit` and `listen` |

## Status machine

```text
            claim                 apply(Commit)
Runnable ───────────► Running ─────────────────► Waiting(reason) | Completed | Faulted
   ▲                     │                               │
   │  sweep (lease lapsed)                               │ condition met (timer due,
   └─────────────────────┘                               │ child done, inbox satisfied)
                                                         ▼
                                                     Runnable
Cancelled: set by a parent's commit (`cancel` list) or an operator; terminal.
```

`claim` takes `Runnable`, or `Waiting(Timer)` whose `wake_at` is due. The
other `Waiting` reasons are flipped to `Runnable` by the engine when it
observes the condition: a child's terminal commit, an inbox that satisfies
the `ListenSpec`.

## One engine step

```text
1. instances = store.claim(worker, now, now + lease, limit)
2. for each instance:
   a. definition = definitions.get(instance.definition)
   b. continuation = decode(instance.continuation)        (None → start / start_at)
   c. outcome = core::resume(definition, continuation, event, options{now})
        event is Tick after a claim; Output/Fault after an executor; Release
        after a timer, a join, a child, or an inbox
   d. loop while outcome is Blocked(Activity):
        write StepResult::Pending            ← engine.commit, lease held
        output = executor.execute(task)      (call: http uses core::http_call)
        upsert StepResult::{Output|Fault}    ← same (instance, key, attempt)
        outcome = core::resume(..., Output|Fault)
      a still-Pending row on the next claim is re-run at attempt + 1 only
      when the executor is idempotent; otherwise the instance is Faulted and
      the row is upserted to Unknown
   e. commit = translate(outcome):
        Completed            → status Completed, output, continuation None
        Faulted              → status Faulted, fault, continuation None
        Blocked(Timer)       → Waiting(Timer), wake_at = task_started + detail.after
        Blocked(Retry)       → Waiting(Timer), wake_at = pause.retry_at(task_started)
        Blocked(Join)        → Waiting(Join),  spawn_children = detail.branches
        Blocked(Child)       → Waiting(Child), spawn_children = [detail.child]
        Blocked(Events)      → Waiting(Events), events_in.subscribe(detail.listen)
   f. engine.commit(commit, signal.offset)   ← store.apply, then wakeup.commit_offset
   g. wakeup.notify(parent or self) as a hint
```

Step `d` is why an engine step may call the interpreter several times but
commits the continuation once per stop. Step `f` is the one place the two
backends are ordered.

## Invariants per trait

### `InstanceStore`

- `claim` is exclusive: two workers never hold the same instance at once.
  Implementations serialize on the instance row (memory: one mutex; SQL:
  `FOR UPDATE SKIP LOCKED`).
- `apply` is all-or-nothing under `expected_version`. On conflict nothing
  changes and the engine drops its work; the next claim redoes it from the
  stored continuation.
- `spawn_children` and `cancel` land in the same atomic unit as the parent's
  status change. A parent is never `Waiting(Join)` without its children
  existing.
- `sweep` is read-only.

### `Wakeup`

- Replayable from any retained offset. Losing the whole bus loses latency,
  never correctness.
- `commit_offset` is idempotent and monotonic.

### `ActivityExecutor`

- Receives everything in `TaskExecution`; does not read the store.
- Returns `ActivityError::Fault` for failures the workflow may catch and
  `ActivityError::Executor` for its own breakage.
- `idempotent()` is a promise about the side effect, not about the code.
  Default false.

### `DefinitionSource`

- `get` by key is stable forever: the same key returns the same tree.
- `latest` by identity may change between calls; the engine captures the
  key at child creation.

### `EventSource`

- The inbox survives the instance being unclaimed. Events are removed only
  by `ack`, which the engine calls in the same step that resumes the run.

### `Clock`

- The engine reads it once per step and passes that reading everywhere.
  Two reads inside one step are a bug.

## Fork and child instances

A `fork` block carries `BlockDetail::Branches`. The engine creates one child
instance per branch with `parent = { id, branch: i }` and starts each with
`core::start_at(definition, branch.position, input)`. The parent waits with
`Waiting(Join)`. When a child commits a terminal status, the engine
re-evaluates the parent from `store.children(parent)`:

- cooperative: all `Completed` → `Release(outputs in branch order)`; any
  `Faulted` → parent `Faulted`, siblings cancelled;
- compete: first `Completed` → `Release(its output)`, siblings cancelled;
  all `Faulted` → parent `Faulted`.

Re-evaluation is idempotent and guarded by the parent's CAS, so two children
finishing at once produce one release.

`run: workflow` uses the same mechanism with `parent.branch = None` and a
definition resolved through `DefinitionSource::latest` at creation time.
`await: false` releases the parent immediately with the child id.

## What the journal is for

`StepResult` rows are for durations, retry counts, and post-mortems. The
engine writes them and never reads them to decide what to run next. The
single exception is recovery of a `Pending` row, which decides between
re-running an idempotent executor and faulting the instance; even then the
continuation, not the journal, says where the run is.
