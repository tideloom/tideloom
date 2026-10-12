use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tideloom_core::ListenSpec;

use crate::InstanceId;

/// A CloudEvent as the runtime moves it around.
///
/// Context attributes are flat strings, which is what `listen` filters match
/// on. `data` is the payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CloudEvent {
    /// `id`.
    pub id: String,
    /// `type`.
    pub r#type: String,
    /// `source`.
    pub source: String,
    /// `subject`, when set.
    pub subject: Option<String>,
    /// `time`, RFC 3339, when set.
    pub time: Option<String>,
    /// Every other context attribute, including extensions.
    pub extensions: BTreeMap<String, String>,
    /// `data`.
    pub data: Value,
}

impl CloudEvent {
    /// A context attribute by name, including `id`, `type`, `source`, and `subject`.
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<&str> {
        match name {
            "id" => Some(&self.id),
            "type" => Some(&self.r#type),
            "source" => Some(&self.source),
            "subject" => self.subject.as_deref(),
            "time" => self.time.as_deref(),
            other => self.extensions.get(other).map(String::as_str),
        }
    }
}

/// Why an event call failed.
#[derive(Debug, thiserror::Error)]
pub enum EventError {
    /// Anything the backend could not do.
    #[error("event backend: {0}")]
    Backend(String),
}

/// Where `emit` publishes.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Publish one event. Returns once the backend accepted it.
    async fn publish(&self, event: CloudEvent) -> Result<(), EventError>;
}

/// Where `listen` subscribes and collects.
///
/// The source keeps a per-instance inbox. Events that match a subscription
/// land in the inbox whether or not a worker holds the instance, so an event
/// that arrives before the worker re-claims the instance is not lost. The
/// engine reads the inbox after `claim`, and clears it in the same step that
/// resumes the run; a crash in between leaves the inbox intact.
#[async_trait]
pub trait EventSource: Send + Sync {
    /// Start collecting events matching `spec` for `instance`. Idempotent.
    async fn subscribe(&self, instance: &InstanceId, spec: ListenSpec) -> Result<(), EventError>;

    /// Stop collecting and drop the inbox.
    async fn unsubscribe(&self, instance: &InstanceId) -> Result<(), EventError>;

    /// Events collected so far, oldest first. Does not clear them.
    async fn inbox(&self, instance: &InstanceId) -> Result<Vec<CloudEvent>, EventError>;

    /// Remove `ids` from the inbox after the run consumed them.
    async fn ack(&self, instance: &InstanceId, ids: &[String]) -> Result<(), EventError>;
}
