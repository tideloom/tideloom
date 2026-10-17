use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tideloom_core::Fault;

use crate::ActivityError;
use crate::ActivityExecutor;
use crate::CloudEvent;
use crate::DefinitionSource;
use crate::EventSink;
use crate::TaskExecution;
use crate::TaskKind;

/// Publishes the `emit` task's event.
///
/// `emit.event.with` is evaluated by the interpreter when the block is
/// created, in the same scope as `set`. This executor reads that
/// [`tideloom_core::BlockDetail::Emit`] value and turns it into a CloudEvent. `source` and
/// `type` are required. A missing `id` is generated. The published event, as
/// JSON, is the task output so a later `output.as` can read it.
///
/// Publishing is not idempotent: a retried `emit` publishes again.
pub struct EmitExecutor {
    events: Arc<dyn EventSink>,
    definitions: Arc<dyn DefinitionSource>,
}

impl EmitExecutor {
    /// Publish through `events`. The node is resolved from `definitions`.
    #[must_use]
    pub fn new(events: Arc<dyn EventSink>, definitions: Arc<dyn DefinitionSource>) -> Self {
        Self {
            events,
            definitions,
        }
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

    async fn execute(&self, task: TaskExecution) -> Result<Value, ActivityError> {
        let definition = self
            .definitions
            .get(&task.definition)
            .await
            .map_err(|error| ActivityError::Executor(error.to_string()))?;
        let node = definition.get(task.key.position()).ok_or_else(|| {
            ActivityError::Executor(format!(
                "emit task {} is not in the definition",
                task.key.position()
            ))
        })?;
        // The engine's block already holds this value. `TaskExecution` does
        // not, so a direct `execute` evaluates the node with the same
        // function and the same `$input` / `$context` / `$workflow` / `$task`
        // scope. Enclosing `for` bindings are on the block, not on this task.
        let with = evaluated_emit(node, &task)?;
        let event = Self::event_from(task.key.position(), &with)?;
        let output = serde_json::to_value(&event).map_err(|error| {
            ActivityError::Executor(format!("failed to encode emitted event: {error}"))
        })?;
        self.events
            .publish(event)
            .await
            .map_err(|error| ActivityError::Executor(error.to_string()))?;
        Ok(output)
    }
}

/// `BlockDetail::Emit` for a caller that has the node but not the block.
fn evaluated_emit(
    node: &tideloom_core::Node,
    task: &TaskExecution,
) -> Result<Value, ActivityError> {
    let position = task.key.position();
    // Missing `with` is an empty object, matching the interpreter. The
    // CloudEvent conversion then reports the missing `source` and `type`.
    let owned = node
        .body()
        .pointer("/emit/event/with")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let with = &owned;
    if !with.is_object() {
        return Err(ActivityError::Fault(Fault::runtime(
            position,
            "emit.event.with must be an object",
        )));
    }
    let vars = tideloom_core::emit_scope(node, &task.input, &task.context, &task.workflow_input);
    tideloom_core::evaluate_data(with, &task.input, &vars)
        .map_err(|message| ActivityError::Fault(Fault::expression(position, message)))
}
