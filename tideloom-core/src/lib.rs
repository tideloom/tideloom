//! Build a Serverless Workflow document into an immutable definition, then
//! run that tree with an explicit, serializable continuation.
//!
//! [`Definition`] is the immutable tree (identity, dsl, content hash, nodes).
//! [`start`] runs control flow inline from the root and stops at the first
//! blocking or effectful block. The stop is a [`Continuation`]: the frames of
//! every enclosing control-flow task plus the pending block. [`resume`] takes
//! that value and a [`Resume`] event and continues from exactly there; nothing
//! is replayed from the root. The runtime engine resumes with that result. A
//! `try` retry pause carries its backoff delay. Workflow and task timeouts
//! fault when a supplied clock passes a recorded start. Nothing here sleeps,
//! and nothing here sends a request. The model is ADR-0001 in the repository
//! `docs/` directory.
//!
//! This crate does not talk to a broker or store a run. The expressions it
//! evaluates are a small subset, not full jq. An `emit` block carries
//! `emit.event.with` already evaluated. A `call: http` block carries no
//! request; the runtime executor sends it.

#![warn(missing_docs)]

mod continuation;
mod definition;
mod error;
pub(crate) mod expr;
mod fault;
mod hash;
mod key;
mod node;
mod retry;
mod spec;
mod walk;

pub use continuation::Continuation;
pub use continuation::Resume;
pub use continuation::SCHEMA;
pub use definition::Definition;
pub use error::BuildError;
pub use expr::evaluate_data;
pub use fault::Fault;
pub use hash::ContentHash;
pub use hash::canonical_json;
pub use key::Counter;
pub use key::ExecutionKey;
pub use key::TaskKey;
pub use node::CallKind;
pub use node::FlowDirective;
pub use node::Identity;
pub use node::Node;
pub use node::NodeKind;
pub use node::RunKind;
pub use retry::JitterSample;
pub use retry::Timestamp;
pub use spec::BlockDetail;
pub use spec::Branch;
pub use spec::ChildRef;
pub use spec::EventFilter;
pub use spec::ListenSpec;
pub use spec::ListenStrategy;
pub use walk::Block;
pub use walk::Outcome;
pub use walk::Pause;
pub use walk::WalkOptions;
pub use walk::resume;
pub use walk::resume_with;
pub use walk::start;
pub use walk::start_at;
pub use walk::start_at_with;
pub use walk::start_with;
