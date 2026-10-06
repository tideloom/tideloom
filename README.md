# Tideloom

Tideloom is a Rust runtime for the [Serverless Workflow DSL](https://github.com/serverlessworkflow/specification) (Open Workflow Specification 1.0).

This repository builds a workflow file into an immutable definition tree. It does not run tasks, evaluate jq, schedule work, or replay an execution. The execution model is [ADR-0001](docs/ADR-0001-serverless-workflow-execution-model.md).

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

The workspace layout matches this repository's existing FastLabs-style shape: one library crate, `tideloom-core`, beside the virtual-workspace `Cargo.toml`, with `rustfmt.toml`, `taplo.toml`, and `.github/workflows/ci.yml`.
