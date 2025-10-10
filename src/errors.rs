use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error, Serialize, Deserialize, Clone)]
pub enum WorkflowError {
    #[error("task error: {message}")]
    Task { message: String },
    #[error("unexpected: {0}")]
    Unexpected(String),
}
