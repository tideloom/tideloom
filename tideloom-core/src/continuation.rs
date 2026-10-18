//! Resume state of one workflow run.
//!
//! A [`Continuation`] is the run at the moment it stopped on a block: the
//! `$context`, the frames of every enclosing control-flow task, and the
//! pending block itself. It is a value. Serialize it, store it, and hand it
//! back to [`resume`](crate::resume) with what happened. Nothing is replayed
//! from the root.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

use crate::Fault;
use crate::Timestamp;
use crate::key::Counter;
use crate::key::ExecutionKey;
use crate::key::TaskKey;
use crate::walk::Pause;

/// Persistence format of [`Continuation`]. Bump it when a frame changes shape.
pub const SCHEMA: u16 = 1;

/// Which task list a [`SeqFrame`] runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Body {
    /// The workflow `do` list.
    Root,
    /// A `do` task's list. The frame owner's `then` applies when it finishes.
    Do,
    /// One `for` iteration. The [`ForFrame`] below decides what comes next.
    Iteration,
    /// One `try` attempt. The [`TryFrame`] below catches faults.
    Attempt,
    /// The `catch.do` list of a `try`.
    Catch,
}

/// Where a `try` is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum TryPhase {
    /// Running the `try` list.
    Body,
    /// Paused for retry backoff. The pending block is the `try` task.
    Backoff,
    /// Running the `catch.do` list.
    Catch,
}

/// A task list in progress.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SeqFrame {
    /// Pointer of the task that owns the list.
    pub owner: String,
    pub body: Body,
    /// Index of the child currently running.
    pub next: usize,
    /// `then` jumps taken so far. Contributes [`Counter::Pass`] when non-zero.
    pub pass: u64,
    /// Transformed input of the owner. Only a [`Body::Do`] owner reads it.
    pub input: Value,
}

/// A `for` loop in progress.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ForFrame {
    pub position: String,
    pub input: Value,
    pub items: Vec<Value>,
    pub index: u64,
    pub each: String,
    pub at: String,
}

/// A `try` in progress.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct TryFrame {
    pub position: String,
    pub input: Value,
    pub attempt: u64,
    pub phase: TryPhase,
    /// The fault being handled. Bound as `$error` (or `catch.as`) while set.
    pub fault: Option<Fault>,
    pub error_as: String,
    /// When the first retry pause was observed. Starts the `limit.duration` window.
    pub retry_started: Option<Timestamp>,
}

/// A `use.functions` call in progress.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct FunctionFrame {
    pub position: String,
    pub input: Value,
}

/// One enclosing control-flow task.
///
/// Outermost first in [`Continuation::frames`]. Leaf tasks never have a
/// frame; the one that paused is the pending block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum Frame {
    Seq(SeqFrame),
    For(ForFrame),
    Try(TryFrame),
    Function(FunctionFrame),
}

impl Frame {
    /// Counter this frame contributes to the execution key.
    pub fn counter(&self) -> Option<Counter> {
        match self {
            Self::Seq(seq) if seq.pass > 0 => Some(Counter::Pass(seq.pass)),
            Self::Seq(_) | Self::Function(_) => None,
            Self::For(frame) => Some(Counter::Loop(frame.index)),
            Self::Try(frame) => Some(Counter::Attempt(frame.attempt)),
        }
    }

    /// Variables this frame binds for expressions below it.
    pub fn bindings(&self, into: &mut Map<String, Value>) {
        match self {
            Self::For(frame) => {
                let item = usize::try_from(frame.index)
                    .ok()
                    .and_then(|index| frame.items.get(index))
                    .cloned()
                    .unwrap_or(Value::Null);
                into.insert(frame.each.clone(), item);
                into.insert(frame.at.clone(), Value::from(frame.index));
            }
            Self::Try(frame) => {
                if let Some(fault) = &frame.fault {
                    into.insert(frame.error_as.clone(), fault.to_value());
                }
            }
            Self::Seq(_) | Self::Function(_) => {}
        }
    }
}

/// What the run was started on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) enum Root {
    /// The workflow itself: `input.from`, `output.as`, and `timeout` apply.
    Workflow,
    /// A single task, started with [`start_at`](crate::start_at). Its output is the run output.
    Task {
        /// JSON Pointer of the task.
        position: String,
    },
}

/// The block a run stopped on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Pending {
    pub position: String,
    pub input: Value,
    pub pause: Pause,
}

/// A run stopped on a block, ready to be resumed.
///
/// `Serialize` and `Deserialize` are the storage form. Two continuations are
/// equal when they would resume identically.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Continuation {
    pub(crate) schema: u16,
    pub(crate) root: Root,
    pub(crate) workflow_input: Value,
    pub(crate) context: Map<String, Value>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) pending: Pending,
    pub(crate) workflow_started: Option<Timestamp>,
    pub(crate) task_started: Option<Timestamp>,
}

impl Continuation {
    /// Persistence format this value was written with. See [`SCHEMA`].
    #[must_use]
    pub fn schema(&self) -> u16 {
        self.schema
    }

    /// The task this run was started on with [`start_at`](crate::start_at), if any.
    ///
    /// `None` means the run started at the workflow root.
    #[must_use]
    pub fn root_task(&self) -> Option<&str> {
        match &self.root {
            Root::Workflow => None,
            Root::Task { position } => Some(position),
        }
    }

    /// JSON Pointer of the pending block.
    #[must_use]
    pub fn position(&self) -> &str {
        &self.pending.position
    }

    /// Why the run stopped.
    #[must_use]
    pub fn pause(&self) -> Pause {
        self.pending.pause
    }

    /// Transformed input of the pending block.
    #[must_use]
    pub fn input(&self) -> &Value {
        &self.pending.input
    }

    /// Counters of the enclosing frames, outermost first.
    #[must_use]
    pub fn execution_key(&self) -> ExecutionKey {
        ExecutionKey::new(
            self.frames
                .iter()
                .filter_map(Frame::counter)
                .collect::<Vec<_>>(),
        )
    }

    /// Position plus execution key of the pending block.
    #[must_use]
    pub fn key(&self) -> TaskKey {
        TaskKey::new(self.pending.position.clone(), self.execution_key())
    }

    /// Number of enclosing control-flow frames.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// `$workflow.input`.
    #[must_use]
    pub fn workflow_input(&self) -> &Value {
        &self.workflow_input
    }

    /// Expression scope of the pending task, including enclosing frame bindings.
    ///
    /// This is the scope `set` uses: `.` is `input`, and the map holds `$input`,
    /// `$context`, `$workflow`, `$task`, plus `$item` / `$index` from a `for`
    /// and `$error` from a `try`. An executor that evaluates its own fields,
    /// such as HTTP `with`, uses this. An `emit` payload is already evaluated
    /// and lives on [`crate::Block::detail`].
    #[must_use]
    pub fn scope(&self, node: &crate::Node, input: &Value) -> BTreeMap<String, Value> {
        let mut map = BTreeMap::new();
        map.insert("context".to_string(), Value::Object(self.context.clone()));
        map.insert("input".to_string(), input.clone());
        map.insert(
            "workflow".to_string(),
            serde_json::json!({ "input": self.workflow_input }),
        );
        map.insert(
            "task".to_string(),
            serde_json::json!({
                "name": node.name(),
                "reference": node.position(),
                "input": input,
            }),
        );
        let mut bindings = Map::new();
        for frame in &self.frames {
            frame.bindings(&mut bindings);
        }
        map.extend(bindings);
        map
    }

    /// `$context` at the stop.
    #[must_use]
    pub fn context(&self) -> Value {
        Value::Object(self.context.clone())
    }

    /// When the workflow run started, for the workflow `timeout`.
    #[must_use]
    pub fn workflow_started(&self) -> Option<Timestamp> {
        self.workflow_started
    }

    /// Remember when the workflow run started. The first instant stays.
    pub fn start_workflow(&mut self, at: Timestamp) {
        self.workflow_started.get_or_insert(at);
    }

    /// When the runner first observed the pending block.
    #[must_use]
    pub fn task_started(&self) -> Option<Timestamp> {
        self.task_started
    }

    /// Remember when the pending block was first observed. The first instant stays.
    ///
    /// A new block clears it, so the deadline of each pause starts from its
    /// own first observation.
    pub fn start_task(&mut self, at: Timestamp) {
        self.task_started.get_or_insert(at);
    }
}

impl fmt::Display for Continuation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.key(), formatter)
    }
}

/// What happened to the pending block.
#[derive(Clone, Debug, PartialEq)]
pub enum Resume {
    /// An effectful task finished. This is its raw output, before `output.as`.
    Output(Value),
    /// An effectful task failed. A `try` may catch it.
    Fault(Fault),
    /// A blocking pause (`wait`, `listen`, `fork` join, or a retry backoff)
    /// is over. The value is what the task continues with; a retry backoff
    /// ignores it.
    Release(Value),
    /// Nothing happened. Re-check workflow, task, attempt, and retry-window
    /// deadlines against `WalkOptions::with_now`. Without a reached deadline
    /// the same block comes back.
    Tick,
}
