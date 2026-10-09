# Tideloom

Tideloom is a Rust runtime for the [Serverless Workflow DSL](https://github.com/serverlessworkflow/specification) (Open Workflow Specification 1.0).

This repository builds a workflow file into an immutable definition tree and re-walks that tree from the root. Resume state is a result log, not a node stack. The walk runs control flow inline and returns the next blocking or effectful block. `drive` performs a blocked `call: http`, writes the result into that log, and walks again. It does not talk to a broker or store a run. The execution model is [ADR-0001](docs/ADR-0001-serverless-workflow-execution-model.md).

## Build

Rust 1.90 or newer (edition 2024). `rust-toolchain.toml` selects stable.

```sh
cargo test
cargo build -p tideloom-core
```

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

## Walk

`tideloom_core::walk` takes a definition, the workflow input, and a `ResultLog`. It always starts at the root.

- `set`, `do`, `switch`, `for`, `raise`, `try` (until a retry backoff), and `call` of a `use.functions` entry run inline.
- The walk stops at the next block boundary and returns a `Block`: position, execution key, pause reason, transformed input, `$context`, and whether the block is effectful.
- Pause reasons are `Activity`, `Timer` (`wait`), `Events` (`listen`), `Join` (`fork`), `Retry` (`try` backoff), and `Child` (`run workflow`). A retry pause includes the computed wait. See [Retry backoff](#retry-backoff).
- An effectful output or fault is stored with `ResultLog::record_output` / `record_fault`, keyed by position plus the ancestor `for` indexes and `try` attempts (`loop:0/attempt:1`). A later walk skips that task and continues with the logged value. Only those entries are `task_execution` results.
- `wait`, `listen`, `fork`, and a retry backoff are not effectful. `ResultLog::release` marks them finished so the next walk can pass them. `fork` branches and `listen.foreach` are not walked.

Expressions are a closed subset used by `if`, `switch.when`, `for.in`, `while`, `input.from`, `output.as`, `export.as`, and `${ ... }` inside `set`: paths, literals, comparisons, arithmetic, `and` / `or` / `not`, arrays, objects, and `|`. Full jq is not implemented.

```rust
let outcome = tideloom_core::walk(&definition, &input, &log);
```

## Retry backoff

When `catch.retry` wants another try, `walk` returns `Pause::Retry` and stops. It does not sleep. `attempt` is the zero-based try that just failed. `delay` is the wait before the next one.

`Pause::retry_at(started_at)` is `started_at + delay`. The runner keeps `started_at` from the moment it first observed that pause. A later walk with the same inputs returns the same delay; it does not move `started_at`. `ResultLog::release` on the pause key makes the next walk run the try body under the next attempt key (`attempt:1`, and so on).

| Backoff | Wait |
| --- | --- |
| omitted or `constant` | `delay` |
| `linear` | `delay + increment * attempt`. A missing `increment` defaults to `delay`, so the waits are `delay`, `2 * delay`, `3 * delay`, ... |
| `exponential` | `delay * 2^attempt`, exponent capped at 32. The first retry waits `delay`, then the wait doubles. |

A missing `delay` is 0, and the walk still pauses so the attempt key can advance on release. `delay` is an ISO 8601 string (`PT3S`; a year is 365 days and a month is 30 days), an object (`days`, `hours`, `minutes`, `seconds`, `milliseconds`), or a `${ ... }` expression that evaluates to an ISO 8601 string.

`jitter.from` and `jitter.to` are added after the backoff. `walk` uses `from`, so the delay does not change between walks. `walk_with` and `WalkOptions::with_jitter` pick another point: `JitterSample::FROM` is `from`, `JitterSample::TO` is `to`, and `JitterSample::new(parts_per_million)` is a point in between. Pass the same sample every time that pause is recomputed.

`limit.attempt.count` is unchanged. `limit.attempt.duration` and `limit.duration` are not enforced. `drive` returns the retry pause instead of waiting.

## Timeouts

`timeout.after` on the workflow or on a task is a duration: an ISO 8601 string, an object (`days`, `hours`, `minutes`, `seconds`, `milliseconds`), a `${ ... }` expression that evaluates to an ISO 8601 string, or the name of an entry in `use.timeouts`. The walk does not sleep.

Record the start with `ResultLog::start_workflow` or `ResultLog::start_task` (the block key). The first instant stays. Pass that clock reading to `WalkOptions::with_now`. When `now` is at or past the start plus `after`, the walk returns a timeout fault: status 408, type `https://open-workflow-specification.org/spec/1.0.0/errors/timeout`. A workflow timeout is instance `/`. A task timeout is the task's JSON Pointer, so `try` can catch it. A logged output or a released pause is kept.

`Block::timeout` is the task duration. `Block::timeout_at(started_at)` is the deadline. `walk` without `now` still returns the block and does not enforce the deadline. `drive_with` uses the same clock and faults before it sends an HTTP call that has already timed out.

`Fault::configuration`, `validation`, `authentication`, `authorization`, and `timeout` are the standard error types from the DSL, alongside the existing expression, communication, and runtime faults.

## HTTP call

`tideloom_core::drive` re-walks until the workflow completes, faults, or stops on a block that is not `call: http`. For each HTTP activity it sends the request and stores the raw output with `ResultLog::record_output`, or the fault with `record_fault`. The next walk applies `output.as`, `export.as`, and `try`.

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

Not in this slice: `output: raw`, endpoint authentication, HTTPS, URI-template operators other than `{name}`, gRPC, OpenAPI, AsyncAPI, brokers, and a database. The HTTP client keeps its own 10 second cap. Workflow and task timeouts are the walk's clock, described above.

```rust
let outcome = tideloom_core::drive(&definition, &input, &mut log);
```

The workspace layout matches this repository's existing FastLabs-style shape: one library crate, `tideloom-core`, beside the virtual-workspace `Cargo.toml`, with `rustfmt.toml`, `taplo.toml`, and `.github/workflows/ci.yml`.
