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
/// `emit.event.with` is taken from the definition node and evaluated against
/// the task input, `$context`, and `$workflow.input`. `source` and `type` are
/// required. A missing `id` is generated. The published event, as JSON, is
/// the task output so a later `output.as` can read it.
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
        let with = evaluated_with(node.body(), &task)?;
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

fn evaluated_with(body: &Value, task: &TaskExecution) -> Result<Value, ActivityError> {
    let position = task.key.position();
    let with = body
        .get("emit")
        .and_then(|emit| emit.get("event"))
        .and_then(|event| event.get("with"))
        .ok_or_else(|| {
            ActivityError::Fault(Fault::runtime(
                position,
                "emit task is missing `emit.event.with`",
            ))
        })?;
    if !with.is_object() {
        return Err(ActivityError::Fault(Fault::runtime(
            position,
            "emit.event.with must be an object",
        )));
    }
    evaluate_value(with, task)
        .map_err(|message| ActivityError::Fault(Fault::expression(position, message)))
}

/// Evaluate `${ ... }` inside `emit.event.with`.
///
/// The core evaluator is not public, and this MR must not change
/// `tideloom-core`. The subset here is what an emit payload needs: dotted
/// paths from `.`, `$input`, `$context`, and `$workflow`, plus string
/// interpolation. A whole-string `${ ... }` keeps its JSON type.
fn evaluate_value(value: &Value, task: &TaskExecution) -> Result<Value, String> {
    match value {
        Value::String(text) => evaluate_string(text, task),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(evaluate_value(item, task)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(object) => {
            let mut out = serde_json::Map::new();
            for (key, child) in object {
                out.insert(key.clone(), evaluate_value(child, task)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

fn evaluate_string(text: &str, task: &TaskExecution) -> Result<Value, String> {
    if let Some(inner) = sole_interpolation(text) {
        return lookup(inner, task);
    }
    if !text.contains("${") {
        return Ok(Value::String(text.to_string()));
    }
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unclosed interpolation in `{text}`"))?;
        let value = lookup(after[..end].trim(), task)?;
        out.push_str(&stringify(&value));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(Value::String(out))
}

fn sole_interpolation(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix("${")?;
    let inner = rest.strip_suffix('}')?;
    if inner.contains("${") {
        return None;
    }
    Some(inner.trim())
}

fn lookup(path: &str, task: &TaskExecution) -> Result<Value, String> {
    let path = path.trim().trim_start_matches('$');
    let (root, rest) = split_root(path);
    let mut current = match root {
        "" | "." => task.input.clone(),
        "input" => task.input.clone(),
        "context" => task.context.clone(),
        "workflow" => serde_json::json!({ "input": task.workflow_input }),
        other => {
            return Err(format!(
                "emit expressions only support `.`, `$input`, `$context`, and `$workflow`, not `{other}`"
            ));
        }
    };
    for part in rest.split('.').filter(|part| !part.is_empty()) {
        current = match current {
            Value::Object(object) => object.get(part).cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        };
    }
    Ok(current)
}

fn split_root(path: &str) -> (&str, &str) {
    let path = path.trim();
    if let Some(rest) = path.strip_prefix('.') {
        return (".", rest.trim_start_matches('.'));
    }
    match path.split_once('.') {
        Some((root, rest)) => (root, rest),
        None => (path, ""),
    }
}

fn stringify(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tideloom_core::TaskKey;

    use super::evaluate_string;
    use super::evaluate_value;
    use crate::DefinitionKey;
    use crate::InstanceId;
    use crate::TaskExecution;
    use crate::TaskKind;

    fn task() -> TaskExecution {
        TaskExecution {
            instance: InstanceId::from_string("i"),
            definition: DefinitionKey {
                namespace: "default".to_string(),
                name: "paint".to_string(),
                version: "0.1.0".to_string(),
                content_hash: "ab".to_string(),
            },
            key: TaskKey::new("/do/0/notify", tideloom_core::ExecutionKey::new(vec![])),
            kind: TaskKind::Emit,
            attempt: 0,
            input: json!({"order": "A-9", "n": 3}),
            context: json!({"tenant": "acme"}),
            workflow_input: json!({"region": "us"}),
        }
    }

    #[test]
    fn a_whole_interpolation_keeps_its_json_type() {
        let task = task();
        assert_eq!(evaluate_string("${ .n }", &task).unwrap(), json!(3));
        assert_eq!(evaluate_string("${ .order }", &task).unwrap(), json!("A-9"));
        assert_eq!(
            evaluate_string("${ $context.tenant }", &task).unwrap(),
            json!("acme")
        );
        assert_eq!(
            evaluate_string("${ $workflow.input.region }", &task).unwrap(),
            json!("us")
        );
    }

    #[test]
    fn a_partial_interpolation_becomes_text_and_objects_walk() {
        let task = task();
        assert_eq!(
            evaluate_string("order ${ .order }", &task).unwrap(),
            json!("order A-9")
        );
        let with = json!({
            "source": "https://example.test",
            "type": "io.example.paid",
            "data": { "order": "${ .order }", "n": "${ .n }" }
        });
        let evaluated = evaluate_value(&with, &task).unwrap();
        assert_eq!(evaluated["data"]["order"], "A-9");
        assert_eq!(evaluated["data"]["n"], 3);
    }

    #[test]
    fn an_unsupported_root_is_an_expression_error() {
        let error = evaluate_string("${ $task.name }", &task()).unwrap_err();
        assert!(error.contains("task"), "{error}");
    }
}
