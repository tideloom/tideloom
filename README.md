# Tideloom

Tideloom is a Rust runtime for the [Serverless Workflow DSL](https://github.com/serverlessworkflow/specification) (Open Workflow Specification 1.0).

This repository builds a workflow file into an immutable definition tree and runs it with an explicit continuation. `start` runs control flow inline from the root and stops at the first blocking or effectful block; the stop is a serializable `Continuation`. `resume` takes that value and what happened to the block, and continues from exactly there. Nothing is replayed from the root. That is `tideloom-core`, and it is pure: a blocked `call: http` is one request via `http_call`, not a walk. `tideloom-runtime` holds the traits for everything with side effects: the instance store, the wake-up bus, activity executors, definitions, events, and the clock, composed by an `Engine`. The execution model is [ADR-0001](docs/ADR-0001-serverless-workflow-execution-model.md); the runtime traits are described in [docs/runtime-architecture.md](docs/runtime-architecture.md); the work queue is [docs/milestone-m2-single-process-runtime.md](docs/milestone-m2-single-process-runtime.md). Contributors and coding agents start at [AGENTS.md](AGENTS.md).

## Build

Rust 1.90 or newer (edition 2024). `rust-toolchain.toml` selects stable.

```sh
cargo test
cargo build -p tideloom-core
cargo run -p tideloom-runtime --example single_process
```

## Run a workflow

`Engine` is the single-process runner. It claims instances from an `InstanceStore`, resumes the continuation, and commits the result. Time comes from a `Clock`. A test injects `ManualClock` and calls `Engine::drive_until_idle`, which advances that clock to the next `wake_at` instead of sleeping.

The acceptance workflow is `tideloom-runtime/examples/single_process.yaml`. One `Engine` runs it through `set`, `call: http`, a `try` retry, `wait`, a cooperative `fork`, a compete `fork`, `run: workflow`, and an `emit` that a later `listen` consumes. The HTTP call goes to a TCP listener in the same process through `http_call`. The child workflow is a second document in that file, published into `MemoryDefinitions` before the parent starts.

```sh
cargo run -p tideloom-runtime --example single_process
```

The process prints the parent output. `sealed` is true when every stop above completed. The example does not sleep.

## From a spec file to the immutable definition

`tideloom_core::Definition::from_yaml` and `Definition::from_json` parse a document and return a `Definition`:

| Field | Source |
| --- | --- |
| `identity` | `(namespace, name, version)` from `document` |
| `dsl` | `document.dsl` |
| `content_hash` | SHA-256 of the document's canonical JSON |
| `root` | Tree of `Node` values |

Canonical JSON sorts object keys and drops insignificant whitespace, so the same workflow has the same hash in YAML or JSON. A definition is identified by the identity triple plus that hash. The tree built here is not modified in place; publishing another document is a new value.

Each node stores:

- a JSON Pointer, for example `/do/0/validateOrder` (the root pointer is empty)
- the task name
- the task kind
- three independent flags computed at build time: `composite`, `blocking`, `effectful`
- the unevaluated `then` directive (`continue` when omitted)
- child nodes
- the unevaluated source object, including runtime expressions as text

`composite` means the interpreter will navigate into children (`do`, `for`, `fork`, `try`, `switch`, `call` of a function, the workflow root, and `listen` when it has a `foreach` body). `blocking` means the task may pause (`wait`, `listen`, `run` workflow, `fork`, and `try` when `catch.retry` is set). `effectful` means an external side effect (`call` of `http`, `grpc`, `openapi`, `asyncapi`, `a2a`, or `mcp`; `run` of a container, shell, script, or workflow; `emit`).

A node is a block boundary when it is blocking or effectful. Only effectful nodes would later record a `task_execution` row. `set`, `raise`, and `switch` are none of the three. A local `call` whose name is in `use.functions` is inlined at `{call}/_fn`.

```rust
let definition = tideloom_core::Definition::from_yaml(yaml)?;
let task = definition.get("/do/0/validateOrder").unwrap();
let _boundary = task.is_block_boundary();
```

## Start and resume

`tideloom_core::start` takes a definition and the workflow input and runs from the root.

- `set`, `do`, `switch`, `for`, `raise`, `try` (until a retry backoff), and `call` of a `use.functions` entry run inline.
- The run stops at the next block boundary and returns `Outcome::Blocked { block, continuation }`. The `Block` has the position, execution key, pause reason, transformed input, `$context`, and whether the block is effectful. The `Continuation` is everything needed to continue: `$context`, one frame per enclosing control-flow task, and the pending block. It implements `Serialize` / `Deserialize`; store it as the instance state.
- Pause reasons are `Activity`, `Timer` (`wait`), `Events` (`listen`), `Join` (`fork`), `Retry` (`try` backoff), and `Child` (`run workflow`). A retry pause includes the computed wait. See [Retry backoff](#retry-backoff).
- `tideloom_core::resume` takes the definition, the continuation, and a `Resume` event: `Output` or `Fault` for an effectful task, `Release` for `wait`, `listen`, `fork`, and a retry backoff, or `Tick` when only the clock moved. Inline tasks after the block run until the next boundary. Inline tasks before it do not run again.
- The execution key is projected from the frames: `for` indexes, `try` attempts, and `then` jump passes (`loop:0/attempt:1`, `pass:2`). Two visits of the same task in different iterations, attempts, or passes have different keys, so a `then` that jumps back to a `call: http` polls instead of looping on a stale result.
- `Block::detail()` is the pause-specific data a runner needs: the `wait` duration, the `fork` branches, the `run: workflow` target with its evaluated input, or the `listen` subscription (static `with` attributes only; expressions, `correlate`, and `until` fault at block creation).
- `tideloom_core::start_at` runs one task as its own root, which is how a runner starts a `fork` branch as a separate instance. The task output is the run output; `Continuation::root_task` says which task it was.
- `Continuation::schema` is the persistence format version (`tideloom_core::SCHEMA`). A continuation written with another version is refused on resume.
- `listen.foreach` is not walked.

Expressions are a closed jq subset used by `if`, `switch.when`, `for.in`, `while`, `input.from`, `output.as`, `export.as`, and `${ ... }` inside `set`. See [Expressions](#expressions) for the supported filters.

```rust
let Outcome::Blocked { block, continuation } = tideloom_core::start(&definition, &input) else {
    return;
};
let stored = serde_json::to_vec(&continuation)?;
let restored: Continuation = serde_json::from_slice(&stored)?;
let outcome = tideloom_core::resume(&definition, restored, Resume::Output(output));
```

## Expressions

The pure evaluator supports paths (including `$` variables and negative array indexes), JSON literals, comparisons, arithmetic, `and` / `or` / `not`, array and object constructors, and pipes (`|`). It does not invoke jq or perform I/O. Full jq is not implemented: unsupported syntax, unknown functions, wrong argument counts, and invalid input types become expression faults.

| Filter | Behavior |
| --- | --- |
| `map(f)` | Evaluate `f` on each array element or object value and collect its results into an array; for example, `map(.price * 2)`. |
| `select(f)` | Keep the input when `f` is truthy; otherwise emit no result. Use `map(select(.enabled))` to filter a collection. |
| `length` | Array/object size, Unicode character count for strings, absolute value for numbers, or 0 for null; booleans fault. |
| `has(key)` | Test object membership by string key or array membership by integer index, including entries whose value is null; negative indexes return false. |
| `keys` | Sorted object keys or an array's zero-based indexes. |
| `add` | Reduce array elements or object values with `+`; numbers sum, strings/arrays concatenate, objects merge with later keys winning, and null is the identity. Empty collections return null. |
| `tostring` | Leave strings unchanged; serialize any other JSON value as compact JSON text. |
| `tonumber` | Leave numbers unchanged or parse a finite numeric string; other values and invalid strings fault. |
| `ascii_downcase` | Lowercase ASCII letters in a string, leaving non-ASCII characters unchanged. |
| `split(separator)` | Split a string by a literal string separator, preserving empty fields; an empty separator splits into Unicode characters, and empty input returns `[]`. |
| `join(separator)` | Join array scalars with a string separator; null becomes empty text, numbers/booleans become JSON text, and nested arrays/objects fault. |
| `startswith(prefix)` | Test whether a string starts with a string prefix. |
| `endswith(suffix)` | Test whether a string ends with a string suffix. |
| `contains(value)` | String substring, recursive array containment (every requested element matches some input element), recursive object containment, or equality for same-type scalars. Top-level type mismatches fault. |
| `if condition then yes else no end` | Evaluate only the chosen branch against the original input; conditionals may nest. |
| `left // right` | Keep the left filter's non-null/non-false results; evaluate the right filter against the original input only when none remain. Errors on the left are not swallowed. |
| `[.[] \| f]` | Iterate array elements or object values, apply `f`, and collect all results, including an empty stream as `[]`. |

`select`, expression conditionals, and `//` use jq truthiness: only null and false are falsy; 0, empty strings, and empty collections are truthy. Existing `and` / `or` / `not` remain boolean-only, and integer arithmetic retains checked overflow and integer division.

Pipes apply their right filter to each left result. Array constructors and `map` collect streams; an expression at the workflow boundary must produce exactly one JSON value. Uncollected zero/multiple results fault rather than silently losing data. For example, `[.items[] | select(.enabled) | .name | ascii_downcase]` returns an array, whereas `.items[]` faults if it emits zero or more than one item. `set` strings without `${ ... }` remain literal strings.

## Retry backoff

When `catch.retry` wants another try, the run returns `Pause::Retry` and stops. It does not sleep. `attempt` is the zero-based try that just failed. `delay` is the wait before the next one.

`Pause::retry_at(started_at)` is `started_at + delay`. The runner keeps `started_at` on the continuation with `Continuation::start_task`. The delay is computed once, when the pause is created, and stored on the continuation; a later `Resume::Tick` returns the same pause. `Resume::Release` on that continuation runs the try body under the next attempt key (`attempt:1`, and so on).

| Backoff | Wait |
| --- | --- |
| omitted or `constant` | `delay` |
| `linear` | `delay + increment * attempt`. A missing `increment` defaults to `delay`, so the waits are `delay`, `2 * delay`, `3 * delay`, ... |
| `exponential` | `delay * 2^attempt`, exponent capped at 32. The first retry waits `delay`, then the wait doubles. |

A missing `delay` is 0, and the run still pauses so the attempt key can advance on release. `delay` is an ISO 8601 string (`PT3S`; a year is 365 days and a month is 30 days), an object (`days`, `hours`, `minutes`, `seconds`, `milliseconds`), or a `${ ... }` expression that evaluates to an ISO 8601 string.

`jitter.from` and `jitter.to` are added after the backoff. `start` and `resume` use `from`. `start_with`, `resume_with`, and `WalkOptions::with_jitter` pick another point: `JitterSample::FROM` is `from`, `JitterSample::TO` is `to`, and `JitterSample::new(parts_per_million)` is a point in between. The sample is read when the pause is created.

`limit.attempt.count` is unchanged. `limit.attempt.duration` is how long one attempt's paused task may run, from the task start of that block. `limit.duration` is how long retries may continue, from the task start of the first retry pause. Both are checked on `Resume::Tick` against `WalkOptions::with_now` and fault or stop with the same clock as timeouts; neither sleeps. Without `now`, or before the start is recorded, they are not enforced. `start` returns the retry pause instead of waiting. The runtime claims that instance again when `wake_at` is due.

## Timeouts

`timeout.after` on the workflow or on a task is a duration: an ISO 8601 string, an object (`days`, `hours`, `minutes`, `seconds`, `milliseconds`), a `${ ... }` expression that evaluates to an ISO 8601 string, or the name of an entry in `use.timeouts`. The run does not sleep.

Starts live on the continuation. `start_with` a `now` records the workflow start, and a new block records the task start; `Continuation::start_workflow` and `Continuation::start_task` set them when the run had no clock. The first instant stays. Pass the current clock reading to `WalkOptions::with_now` and resume with `Resume::Tick`. When `now` is at or past the start plus `after`, the run returns a timeout fault: status 408, type `https://open-workflow-specification.org/spec/1.0.0/errors/timeout`. A workflow timeout is instance `/` and is checked on every resume. A task timeout is the task's JSON Pointer, so `try` can catch it. An `Output` or `Release` that arrives after the deadline is kept.

`Block::timeout` is the task duration. `Block::timeout_at(started_at)` is the deadline. A resume without `now` still returns the block and does not enforce the deadline. The engine ticks first and faults before it sends an HTTP call that has already timed out.

`Fault::configuration`, `validation`, `authentication`, `authorization`, and `timeout` are the standard error types from the DSL, alongside the existing expression, communication, and runtime faults.

## HTTP call

`tideloom_core::http_call` performs one blocked `call: http` and returns the raw output, before task `output.as`. The runtime engine resumes with `Resume::Output` or `Resume::Fault`, so `output.as`, `export.as`, and `try` see the result. It does not sleep for a retry backoff: that pause is claimed again when `wake_at` is due.

`with` accepts:

| Field | Shape |
| --- | --- |
| `method` | Alphabetic token. Omitted means `GET`. |
| `endpoint` | String or `{ uri }`. `${ ... }` is interpolated, then `{name}` is filled from a top-level task-input field. |
| `headers` | Object of strings, numbers, booleans, or null. |
| `query` | Same, or an array of those scalars repeated as one key. |
| `body` | Any JSON value. Sent as JSON with `Content-Type: application/json` when that header is absent. |
| `output` | `content` (default) or `response`. |

`content` is parsed JSON when the response content type is JSON, a string for other text, and null when the body is empty. `response` is `{ request, statusCode, headers, content }`. Status codes outside 200–299 become a communication fault and are not returned as output. Redirects are not followed.

Not in this slice: `output: raw`, endpoint authentication, HTTPS, URI-template operators other than `{name}`, gRPC, OpenAPI, AsyncAPI, brokers, and a database. The HTTP client keeps its own 10 second cap. Workflow and task timeouts are the run's clock, described above.

```rust
let outcome = tideloom_core::start(&definition, &input);
```

The workspace layout matches this repository's existing FastLabs-style shape: one library crate, `tideloom-core`, beside the virtual-workspace `Cargo.toml`, with `rustfmt.toml`, `taplo.toml`, and `.github/workflows/ci.yml`.
