# Tideloom

Tideloom is a Rust runtime for the [Serverless Workflow DSL](https://github.com/serverlessworkflow/specification) (Open Workflow Specification 1.0).

This repository builds a workflow file into an immutable definition tree and re-walks that tree from the root. Resume state is a result log, not a node stack. The walk runs control flow inline and returns the next blocking or effectful block. It does not perform HTTP, talk to a broker, or store a run. The execution model is [ADR-0001](docs/ADR-0001-serverless-workflow-execution-model.md).

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
- Pause reasons are `Activity`, `Timer` (`wait`), `Events` (`listen`), `Join` (`fork`), `Retry` (`try` backoff), and `Child` (`run workflow`).
- An effectful output or fault is stored with `ResultLog::record_output` / `record_fault`, keyed by position plus the ancestor `for` indexes and `try` attempts (`loop:0/attempt:1`). A later walk skips that task and continues with the logged value. Only those entries are `task_execution` results.
- `wait`, `listen`, `fork`, and a retry backoff are not effectful. `ResultLog::release` marks them finished so the next walk can pass them. `fork` branches and `listen.foreach` are not walked.

Expressions are a closed subset used by `if`, `switch.when`, `for.in`, `while`, `input.from`, `output.as`, `export.as`, and `${ ... }` inside `set`: paths, literals, comparisons, arithmetic, `and` / `or` / `not`, arrays, objects, and `|`. Full jq is not implemented.

```rust
let outcome = tideloom_core::walk(&definition, &input, &log);
```

The workspace layout matches this repository's existing FastLabs-style shape: one library crate, `tideloom-core`, beside the virtual-workspace `Cargo.toml`, with `rustfmt.toml`, `taplo.toml`, and `.github/workflows/ci.yml`.
