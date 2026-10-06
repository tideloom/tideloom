//! Build a Serverless Workflow document into an immutable definition tree.
//!
//! This crate parses DSL 1.0 YAML or JSON and returns a [`Definition`]. It does
//! not run tasks, evaluate jq, schedule work, or replay executions. Those steps
//! are outside this build. The model is ADR-0001 in the repository `docs/`
//! directory.

#![warn(missing_docs)]

mod definition;
mod error;
mod hash;
mod node;

pub use definition::Definition;
pub use error::BuildError;
pub use hash::ContentHash;
pub use hash::canonical_json;
pub use node::CallKind;
pub use node::FlowDirective;
pub use node::Identity;
pub use node::Node;
pub use node::NodeKind;
pub use node::RunKind;
