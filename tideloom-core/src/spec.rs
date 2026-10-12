//! What a runner needs to know about a block beyond its pause reason.
//!
//! These values are computed by the interpreter when a block is created and
//! again on every [`Resume::Tick`](crate::Resume::Tick), from the node and the
//! transformed input. They are not stored on the continuation.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// Pause-specific data on a [`Block`](crate::Block).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BlockDetail {
    /// An activity or a retry backoff. The [`Pause`](crate::Pause) says it all.
    None,
    /// `wait`: how long, from the task start.
    Wait {
        /// The `wait` duration. Zero when the task gives none.
        after: Duration,
    },
    /// `fork`: the branches a runner starts as child instances, in order.
    Branches(Vec<Branch>),
    /// `run: workflow`: the child workflow to start.
    Child(ChildRef),
    /// `listen`: what to subscribe to.
    Listen(ListenSpec),
}

/// One `fork` branch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// JSON Pointer of the branch task. Start it with [`start_at`](crate::start_at).
    pub position: String,
    /// Branch task name.
    pub name: String,
}

/// A `run: workflow` target.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChildRef {
    /// `run.workflow.namespace`.
    pub namespace: String,
    /// `run.workflow.name`.
    pub name: String,
    /// `run.workflow.version`.
    pub version: String,
    /// Child workflow input: `run.workflow.input` evaluated against the task
    /// input, or the task input itself.
    pub input: Value,
    /// `run.await`. When false the parent continues as soon as the child is
    /// created, with the child instance id as its output.
    pub await_result: bool,
}

/// How many `listen` filters must match before the task continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListenStrategy {
    /// `to.one`: the single filter.
    One,
    /// `to.any`: the first filter that matches.
    Any,
    /// `to.all`: every filter, one event each.
    All,
}

/// One `listen` event filter, static attributes only.
///
/// `with` values must be literal strings. `${ ... }` expressions and
/// `correlate` are rejected when the block is created.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFilter {
    /// CloudEvents context attributes to match exactly, such as `type` and `source`.
    pub attributes: BTreeMap<String, String>,
}

/// A `listen` subscription.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListenSpec {
    /// Which filters must match.
    pub strategy: ListenStrategy,
    /// The filters, in document order.
    pub filters: Vec<EventFilter>,
    /// `listen.read`: `data` (the default), `envelope`, or `raw`.
    pub read: String,
}
