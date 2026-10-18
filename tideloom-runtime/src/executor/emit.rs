use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use tideloom_core::BlockDetail;
use tideloom_core::Fault;

use crate::ActivityError;
use crate::ActivityExecutor;
use crate::CloudEvent;
use crate::EventSink;
use crate::TaskExecution;
use crate::TaskKind;

/// Publishes the `emit` task's event.
///
/// `emit.event.with` is evaluated by the interpreter when the block is
/// created, in the same scope as `set`, including enclosing `for` and `try`
/// bindings. This executor reads that [`BlockDetail::Emit`] value and turns
/// it into a CloudEvent. It does not evaluate the document again. `source`
/// and `type` are required. A missing `id` is generated. The published event,
/// as JSON, is the task output so a later `output.as` can read it.
///
/// Publishing is not idempotent: a retried `emit` publishes again.
pub struct EmitExecutor {
    events: Arc<dyn EventSink>,
    /// Inboxes the last successful `publish` wrote, keyed by the emitting
    /// instance and task. The engine takes the entry after the outcome
    /// commit and calls [`crate::InstanceStore::wake`]. `publish` itself
    /// does not touch the store.
    landed: Mutex<Vec<Landed>>,
}

/// One `emit` and the instances whose inbox received it.
struct Landed {
    instance: crate::InstanceId,
    key: tideloom_core::TaskKey,
    ids: Vec<crate::InstanceId>,
}

impl EmitExecutor {
    /// Publish through `events`. The payload is [`BlockDetail::Emit`] on the task.
    #[must_use]
    pub fn new(events: Arc<dyn EventSink>) -> Self {
        Self {
            events,
            landed: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn take_landed(
        &self,
        instance: &crate::InstanceId,
        key: &tideloom_core::TaskKey,
    ) -> Vec<crate::InstanceId> {
        let Ok(mut landed) = self.landed.lock() else {
            return Vec::new();
        };
        let position = landed
            .iter()
            .position(|entry| entry.instance == *instance && entry.key == *key);
        position
            .map(|index| landed.swap_remove(index).ids)
            .unwrap_or_default()
    }

    /// The CloudEvent described by an evaluated `emit.event.with` object.
    ///
    /// Missing `id` becomes a fresh id. Missing `source` or `type`, or a
    /// non-string context attribute, is a runtime fault at `position`.
    pub fn event_from(position: &str, with: &Value) -> Result<CloudEvent, ActivityError> {
        let object = with.as_object().ok_or_else(|| {
            ActivityError::Fault(Fault::runtime(
                position,
                "emit.event.with must be an object",
            ))
        })?;
        let source = required_string(object, "source", position)?;
        let event_type = required_string(object, "type", position)?;
        let id = match optional_string(object, "id", position)? {
            Some(id) => id,
            None => fresh_id(),
        };
        let subject = optional_string(object, "subject", position)?;
        let time = optional_string(object, "time", position)?;
        let data = object.get("data").cloned().unwrap_or(Value::Null);
        let mut extensions = BTreeMap::new();
        for (name, value) in object {
            if is_reserved(name) {
                continue;
            }
            let Value::String(text) = value else {
                return Err(ActivityError::Fault(Fault::runtime(
                    position,
                    format!("emit attribute `{name}` must be a string"),
                )));
            };
            extensions.insert(name.clone(), text.clone());
        }
        Ok(CloudEvent {
            id,
            r#type: event_type,
            source,
            subject,
            time,
            extensions,
            data,
        })
    }
}

fn is_reserved(name: &str) -> bool {
    matches!(
        name,
        "id" | "source"
            | "type"
            | "subject"
            | "time"
            | "data"
            | "datacontenttype"
            | "dataschema"
            | "specversion"
    )
}

fn required_string(
    object: &serde_json::Map<String, Value>,
    name: &str,
    position: &str,
) -> Result<String, ActivityError> {
    match optional_string(object, name, position)? {
        Some(text) if !text.is_empty() => Ok(text),
        Some(_) => Err(ActivityError::Fault(Fault::runtime(
            position,
            format!("emit.event.with.{name} must not be empty"),
        ))),
        None => Err(ActivityError::Fault(Fault::runtime(
            position,
            format!("emit.event.with is missing `{name}`"),
        ))),
    }
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    name: &str,
    position: &str,
) -> Result<Option<String>, ActivityError> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ActivityError::Fault(Fault::runtime(
            position,
            format!("emit.event.with.{name} must be a string"),
        ))),
    }
}

fn fresh_id() -> String {
    crate::InstanceId::new().to_string()
}

#[async_trait]
impl ActivityExecutor for EmitExecutor {
    fn kind(&self) -> TaskKind {
        TaskKind::Emit
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, task: TaskExecution) -> Result<Value, ActivityError> {
        let BlockDetail::Emit(with) = &task.detail else {
            return Err(ActivityError::Fault(Fault::runtime(
                task.key.position(),
                "emit task has no evaluated payload",
            )));
        };
        let event = Self::event_from(task.key.position(), with)?;
        let output = serde_json::to_value(&event).map_err(|error| {
            ActivityError::Executor(format!("failed to encode emitted event: {error}"))
        })?;
        // Record who received it. The engine wakes those ids after the
        // outcome commit. Waking here would call the store from the executor.
        let landed = self
            .events
            .publish(event)
            .await
            .map_err(|error| ActivityError::Executor(error.to_string()))?;
        if let Ok(mut recorded) = self.landed.lock() {
            recorded.push(Landed {
                instance: task.instance,
                key: task.key,
                ids: landed,
            });
        }
        Ok(output)
    }
}
