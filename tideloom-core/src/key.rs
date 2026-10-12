use std::fmt;

use serde::Deserialize;
use serde::Serialize;

/// One ancestor counter in an [`ExecutionKey`].
///
/// Outermost first. A `for` contributes [`Counter::Loop`]. A `try` contributes
/// [`Counter::Attempt`] for its body, its retry pause, and its catch body. A
/// task list that took a `then` jump contributes [`Counter::Pass`] so a task
/// reached again by that jump has its own key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Counter {
    /// Zero-based `for` iteration. This is the `at` value.
    Loop(u64),
    /// Zero-based `try` attempt. Zero is the first try, before any retry.
    Attempt(u64),
    /// Number of `then` jumps the enclosing task list has taken. Omitted at zero.
    Pass(u64),
}

/// Ancestor loop indexes, try attempts, and jump passes, outermost first.
///
/// This is the ADR-0001 `execution_key`: the counters of the enclosing
/// frames, projected out of the continuation. The position is stored
/// separately on [`TaskKey`].
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ExecutionKey {
    counters: Vec<Counter>,
}

impl ExecutionKey {
    /// Key with no enclosing `for`, `try`, or jump.
    pub fn empty() -> Self {
        Self {
            counters: Vec::new(),
        }
    }

    /// Build a key from counters, outermost first.
    pub fn new(counters: impl Into<Vec<Counter>>) -> Self {
        Self {
            counters: counters.into(),
        }
    }

    /// Counters, outermost first.
    pub fn counters(&self) -> &[Counter] {
        &self.counters
    }

    /// No enclosing loop, try, or jump.
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }
}

impl fmt::Display for ExecutionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, counter) in self.counters.iter().enumerate() {
            if index > 0 {
                formatter.write_str("/")?;
            }
            match counter {
                Counter::Loop(n) => write!(formatter, "loop:{n}")?,
                Counter::Attempt(n) => write!(formatter, "attempt:{n}")?,
                Counter::Pass(n) => write!(formatter, "pass:{n}")?,
            }
        }
        Ok(())
    }
}

/// Identity of one block-boundary visit: JSON Pointer plus [`ExecutionKey`].
///
/// ADR-0001 identifies a runtime block by `(instance_id, position, execution_key)`.
/// This value has no instance id. Two visits of the same task in different
/// loop iterations, try attempts, or jump passes do not share a key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

    /// Ancestor loop indexes, try attempts, and jump passes.
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
