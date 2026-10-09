use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

use crate::Fault;
use crate::retry::Timestamp;

/// One ancestor counter in an [`ExecutionKey`].
///
/// Outermost frame first. A `for` contributes [`Frame::Loop`]. A `try`
/// contributes [`Frame::Attempt`] for both the try body and the catch body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Frame {
    /// Zero-based `for` iteration. This is the `at` value.
    Loop(u64),
    /// Zero-based `try` attempt. Zero is the first try, before any retry.
    Attempt(u64),
}

/// Ancestor loop indexes and try attempts, outermost first.
///
/// This is the ADR-0001 `execution_key`: the concatenation of those counters,
/// not a node stack. The position is stored separately on [`TaskKey`].
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExecutionKey {
    frames: Vec<Frame>,
}

impl ExecutionKey {
    /// Key with no enclosing `for` or `try`.
    pub fn empty() -> Self {
        Self { frames: Vec::new() }
    }

    /// Build a key from frames, outermost first.
    pub fn new(frames: impl Into<Vec<Frame>>) -> Self {
        Self {
            frames: frames.into(),
        }
    }

    /// Frames, outermost first.
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// No enclosing loop or try.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

impl fmt::Display for ExecutionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, frame) in self.frames.iter().enumerate() {
            if index > 0 {
                formatter.write_str("/")?;
            }
            match frame {
                Frame::Loop(n) => write!(formatter, "loop:{n}")?,
                Frame::Attempt(n) => write!(formatter, "attempt:{n}")?,
            }
        }
        Ok(())
    }
}

/// Identity of one block-boundary visit: JSON Pointer plus [`ExecutionKey`].
///
/// ADR-0001 identifies a runtime block by `(instance_id, position, execution_key)`.
/// This slice has no instance id. Two visits of the same task in different
/// loop iterations or try attempts do not share a key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskKey {
    position: String,
    execution_key: ExecutionKey,
}

impl TaskKey {
    /// Position is a JSON Pointer such as `/do/0/notify`. The root pointer is empty.
    pub fn new(position: impl Into<String>, execution_key: ExecutionKey) -> Self {
        Self {
            position: position.into(),
            execution_key,
        }
    }

    /// JSON Pointer of the boundary node.
    pub fn position(&self) -> &str {
        &self.position
    }

    /// Ancestor loop indexes and try attempts.
    pub fn execution_key(&self) -> &ExecutionKey {
        &self.execution_key
    }
}

impl fmt::Display for TaskKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.execution_key.is_empty() {
            formatter.write_str(&self.position)
        } else {
            write!(formatter, "{}@{}", self.position, self.execution_key)
        }
    }
}

/// Result of an effectful task. This is the `task_execution` payload.
///
/// Blocking nodes that are not effectful are not stored here. Their release
/// lives on [`ResultLog::release`].
#[derive(Clone, Debug, PartialEq)]
pub enum TaskResult {
    /// The runner finished the task. A later walk uses this as the raw output.
    Output(Value),
    /// The runner failed the task. A later walk raises this into `try` or the workflow.
    Fault(Fault),
}

/// Resume state for a re-walk from the root.
///
/// The definition tree is not a stack, and this log is not a stack either.
/// A later [`crate::walk`] starts at the root again. Inline tasks run again.
/// An effectful node is skipped when [`TaskKey`] is present here. A blocking
/// node that is not effectful (`wait`, `listen`, `fork`, or a `try` retry
/// backoff) is skipped when it has been [`ResultLog::release`]d.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultLog {
    effectful: BTreeMap<TaskKey, TaskResult>,
    released: BTreeMap<TaskKey, Value>,
    workflow_started: Option<Timestamp>,
    task_started: BTreeMap<TaskKey, Timestamp>,
}

impl ResultLog {
    /// Empty log. The first walk stops at the first block boundary.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an effectful success. Replaces any previous result at `key`.
    pub fn record_output(&mut self, key: TaskKey, output: Value) {
        self.effectful.insert(key, TaskResult::Output(output));
    }

    /// Record an effectful failure. Replaces any previous result at `key`.
    pub fn record_fault(&mut self, key: TaskKey, fault: Fault) {
        self.effectful.insert(key, TaskResult::Fault(fault));
    }

    /// Record that a blocking, non-effectful pause has finished.
    ///
    /// `output` is the value the walk continues with. A `wait` usually passes
    /// its input through. A `fork` passes the join value supplied by the
    /// runner. A retry backoff ignores the value; pass [`Value::Null`].
    ///
    /// This is not a `task_execution` row. Effectful nodes, including
    /// `run workflow`, use [`ResultLog::record_output`] instead.
    pub fn release(&mut self, key: TaskKey, output: Value) {
        self.released.insert(key, output);
    }

    /// Effectful result at `key`, if the runner has recorded one.
    pub fn effect(&self, key: &TaskKey) -> Option<&TaskResult> {
        self.effectful.get(key)
    }

    /// Output stored by [`ResultLog::release`].
    pub fn released(&self, key: &TaskKey) -> Option<&Value> {
        self.released.get(key)
    }

    /// Remember when the workflow run started.
    ///
    /// The first call wins. A later call does not move the workflow deadline.
    pub fn start_workflow(&mut self, at: Timestamp) {
        if self.workflow_started.is_none() {
            self.workflow_started = Some(at);
        }
    }

    /// Instant recorded by [`ResultLog::start_workflow`].
    #[must_use]
    pub fn workflow_started(&self) -> Option<Timestamp> {
        self.workflow_started
    }

    /// Remember when a paused task started.
    ///
    /// `key` is the block key the runner received. The first call for that key
    /// wins, so a later walk does not move the task deadline.
    pub fn start_task(&mut self, key: TaskKey, at: Timestamp) {
        self.task_started.entry(key).or_insert(at);
    }

    /// Instant recorded by [`ResultLog::start_task`].
    #[must_use]
    pub fn task_started(&self, key: &TaskKey) -> Option<Timestamp> {
        self.task_started.get(key).copied()
    }
}
