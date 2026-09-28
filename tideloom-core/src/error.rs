use std::fmt::Display;

use thiserror::Error;

/// Failure while turning a workflow document into an immutable definition.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BuildError {
    /// YAML or JSON did not parse.
    #[error("invalid workflow {format}: {message}")]
    Parse {
        /// `yaml` or `json`.
        format: &'static str,
        /// Parser message.
        message: String,
    },

    /// A value at `position` was not a JSON object.
    #[error("{position}: expected an object")]
    ExpectedObject {
        /// JSON Pointer of the value.
        position: String,
    },

    /// A value at `position` was not a JSON array.
    #[error("{position}: expected an array")]
    ExpectedArray {
        /// JSON Pointer of the value.
        position: String,
    },

    /// A value at `position` was not a JSON string.
    #[error("{position}: expected a string")]
    ExpectedString {
        /// JSON Pointer of the value.
        position: String,
    },

    /// A required field is absent.
    #[error("{}: missing `{field}`", display_position(position))]
    Missing {
        /// JSON Pointer of the parent.
        position: String,
        /// Field name.
        field: &'static str,
    },

    /// A task-list entry was not a single-key object.
    #[error("{position}: task item must be an object with exactly one task name")]
    TaskItem {
        /// JSON Pointer of the list or entry.
        position: String,
    },

    /// A task object did not declare exactly one task type.
    #[error("{position}: task `{name}` must declare exactly one task type")]
    TaskType {
        /// JSON Pointer of the task.
        position: String,
        /// Task name.
        name: String,
    },

    /// A field is not part of the task or object being built.
    #[error("{position}: unknown field `{field}`")]
    UnknownField {
        /// JSON Pointer of the object.
        position: String,
        /// Field name.
        field: String,
    },

    /// `run` did not select one process kind.
    #[error("{position}: `run` must set exactly one of container, shell, script, or workflow")]
    RunTarget {
        /// JSON Pointer of the task.
        position: String,
    },

    /// A `call` of a local function reached itself through `use.functions`.
    #[error("{position}: function `{name}` calls itself")]
    FunctionCycle {
        /// JSON Pointer of the call task.
        position: String,
        /// Function name.
        name: String,
    },

    /// A switch case omitted `then`.
    #[error("{position}: switch case is missing `then`")]
    SwitchThen {
        /// JSON Pointer of the case.
        position: String,
    },

    /// `switch` was an empty array.
    #[error("{position}: `switch` must contain at least one case")]
    EmptySwitch {
        /// JSON Pointer of the switch task.
        position: String,
    },

    /// `fork.compete` was not a boolean.
    #[error("{position}: `fork.compete` must be a boolean")]
    CompeteType {
        /// JSON Pointer of the flag.
        position: String,
    },

    /// `catch.retry` was neither a policy object nor a name.
    #[error("{position}: `retry` must be a policy object or a name")]
    RetryType {
        /// JSON Pointer of the retry field.
        position: String,
    },
}

fn display_position(position: &str) -> &str {
    if position.is_empty() { "/" } else { position }
}

impl BuildError {
    pub(crate) fn expected_object(position: impl Display) -> Self {
        Self::ExpectedObject {
            position: position.to_string(),
        }
    }

    pub(crate) fn expected_array(position: impl Display) -> Self {
        Self::ExpectedArray {
            position: position.to_string(),
        }
    }

    pub(crate) fn expected_string(position: impl Display) -> Self {
        Self::ExpectedString {
            position: position.to_string(),
        }
    }

    pub(crate) fn missing(position: impl Display, field: &'static str) -> Self {
        Self::Missing {
            position: position.to_string(),
            field,
        }
    }
}
