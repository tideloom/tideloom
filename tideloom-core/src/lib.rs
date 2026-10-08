//! Build a Serverless Workflow document into an immutable definition, then
//! re-walk that tree from the root.
//!
//! [`Definition`] is the immutable tree (identity, dsl, content hash, nodes).
//! [`walk`] runs control flow inline and returns the next blocking or
//! effectful block. Resume state is a [`ResultLog`], not a node stack.
//! [`drive`] performs a blocked `call: http` activity, records the result, and
//! walks again. A `try` retry pause carries its backoff delay; nothing here
//! sleeps. The model is ADR-0001 in the repository `docs/` directory.
//!
//! This crate does not talk to a broker or store a run. The expressions
//! [`walk`] evaluates are a small subset, not full jq. HTTP calls send JSON
//! and read `content` or `response` output.

#![warn(missing_docs)]

mod definition;
mod error;
pub(crate) mod expr;
mod fault;
mod hash;
mod http;
pub(crate) mod log;
mod node;
mod retry;
mod walk;

pub use definition::Definition;
pub use error::BuildError;
pub use fault::Fault;
pub use hash::ContentHash;
pub use hash::canonical_json;
pub use http::drive;
pub use http::http_call;
pub use log::ExecutionKey;
pub use log::Frame;
pub use log::ResultLog;
pub use log::TaskKey;
pub use log::TaskResult;
pub use node::CallKind;
pub use node::FlowDirective;
pub use node::Identity;
pub use node::Node;
pub use node::NodeKind;
pub use node::RunKind;
pub use retry::JitterSample;
pub use retry::Timestamp;
pub use walk::Block;
pub use walk::Outcome;
pub use walk::Pause;
pub use walk::WalkOptions;
pub use walk::walk;
pub use walk::walk_with;
