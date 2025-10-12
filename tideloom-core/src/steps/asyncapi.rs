use serde::Deserialize;
use serde_json::{Map, Value, json};
use serverless_workflow_core::models::authentication::AuthenticationPolicyDefinition;
use serverless_workflow_core::models::task::{CallTaskDefinition, TaskDefinition};

use crate::node::runtime::{StepResult, StepType, WorkflowContext};

#[derive(Debug, Clone, Deserialize)]
pub struct AsyncApiDocument {
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub content: Option<Value>,
}

impl AsyncApiDocument {
    fn to_value(&self) -> Value {
        let mut map = Map::new();
        if let Some(uri) = &self.uri {
            map.insert("uri".into(), Value::String(uri.clone()));
        }
        if let Some(content) = &self.content {
            map.insert("content".into(), content.clone());
        }
        Value::Object(map)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageConfig {
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AsyncApiConfig {
    pub document: AsyncApiDocument,
    #[serde(rename = "operationRef")]
    pub operation_ref: String,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub message: Option<MessageConfig>,
    #[serde(default)]
    pub authentication: AuthenticationPolicyDefinition,
}

#[derive(Debug, Clone)]
pub struct AsyncApiStep {
    config: AsyncApiConfig,
}

impl AsyncApiStep {
    pub fn try_from_task(task: &TaskDefinition) -> StepResult<Self> {
        match task {
            TaskDefinition::Call(call) => Self::try_from_call(call),
            _ => Err("AsyncApiStep expects a `call` task definition".into()),
        }
    }

    pub fn try_from_call(call: &CallTaskDefinition) -> StepResult<Self> {
        if call.call.to_lowercase() != "asyncapi" {
            return Err(format!("expected call 'asyncapi', got '{}'", call.call));
        }

        let with = call
            .with
            .as_ref()
            .ok_or_else(|| "asyncapi call requires a `with` block".to_string())?;

        let mut with_map = Map::new();
        for (key, value) in with {
            with_map.insert(key.clone(), value.clone());
        }

        let config: AsyncApiConfig =
            serde_json::from_value(Value::Object(with_map)).map_err(|err| err.to_string())?;

        Ok(Self { config })
    }

    fn build_request(&self, input: &Value) -> StepResult<Value> {
        let mut request = Map::new();

        let operation_ref = resolve_string(&self.config.operation_ref, input);
        if operation_ref.is_null() {
            return Err("operationRef resolved to null for asyncapi step".into());
        }
        request.insert("operationRef".into(), operation_ref);

        let document_value = resolve_template(&self.config.document.to_value(), input);
        request.insert("document".into(), document_value);

        if let Some(server) = &self.config.server {
            let server_value = resolve_string(server, input);
            if !server_value.is_null() {
                request.insert("server".into(), server_value);
            }
        }

        if let Some(message) = &self.config.message {
            let payload = resolve_template(&message.payload, input);
            request.insert("message".into(), json!({ "payload": payload }));
        }

        let authentication_value =
            serde_json::to_value(&self.config.authentication).unwrap_or(Value::Null);
        if !authentication_value.is_null() {
            let resolved_auth = resolve_template(&authentication_value, input);
            if !resolved_auth.is_null() {
                request.insert("authentication".into(), resolved_auth);
            }
        }

        Ok(Value::Object(request))
    }
}

impl StepType for AsyncApiStep {
    type Input = Value;
    type Output = Value;

    async fn execute(
        &self,
        _ctx: &WorkflowContext,
        input: Self::Input,
    ) -> StepResult<Self::Output> {
        self.build_request(&input)
    }
}

fn resolve_template(template: &Value, context: &Value) -> Value {
    match template {
        Value::String(s) => resolve_string(s, context),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| resolve_template(v, context)).collect())
        }
        Value::Object(map) => {
            let mut resolved = Map::new();
            for (key, value) in map {
                resolved.insert(key.clone(), resolve_template(value, context));
            }
            Value::Object(resolved)
        }
        other => other.clone(),
    }
}

fn resolve_string(input: &str, context: &Value) -> Value {
    let trimmed = input.trim();
    if let Some(expr) = trimmed.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        let expr = expr.trim();
        resolve_expression(expr, context).unwrap_or(Value::Null)
    } else {
        Value::String(input.to_owned())
    }
}

fn resolve_expression(expr: &str, context: &Value) -> Option<Value> {
    if expr.is_empty() {
        return None;
    }

    if expr.starts_with('.') {
        let pointer = build_json_pointer(expr);
        return context.pointer(&pointer).cloned();
    }

    None
}

fn build_json_pointer(expr: &str) -> String {
    let path = expr.trim_start_matches('.');
    if path.is_empty() {
        return String::from("/");
    }
    let segments: Vec<String> = path
        .split('.')
        .map(|segment| segment.replace('~', "~0").replace('/', "~1"))
        .collect();
    format!("/{}", segments.join("/"))
}

#[cfg(test)]
mod tests {
    use serverless_workflow_core::models::workflow::WorkflowDefinition;

    use super::*;

    fn load_first_task(yaml: &str) -> TaskDefinition {
        let workflow: WorkflowDefinition = serde_yaml::from_str(yaml).expect("invalid yaml");
        workflow
            .do_
            .entries
            .first()
            .and_then(|entry| entry.iter().next())
            .map(|(_, task)| task.clone())
            .expect("missing task")
    }

    #[tokio::test]
    async fn asyncapi_step_resolves_payload() {
        let yaml = r#"
 document:
   dsl: '1.0.0'
   namespace: default
   name: call-asyncapi
   version: '1.0.0'
 do:
 - findPet:
     call: asyncapi
     with:
       document:
         uri: https://fake.com/docs/asyncapi.json
       operationRef: findPetsByStatus
       server: staging
       message:
         payload:
           petId: ${ .pet.id }
       authentication:
         bearer:
           token: ${ .token }
 "#;

        let task = load_first_task(yaml);
        let step = AsyncApiStep::try_from_task(&task).expect("asyncapi step");
        let ctx = WorkflowContext::default();
        let input = json!({
            "pet": { "id": 42 },
            "token": "secret-token"
        });

        let output = step
            .execute(&ctx, input)
            .await
            .expect("step should succeed");

        assert_eq!(
            output
                .get("operationRef")
                .and_then(|v| v.as_str())
                .expect("operationRef"),
            "findPetsByStatus"
        );

        let payload = output
            .pointer("/message/payload/petId")
            .expect("payload petId")
            .as_i64()
            .expect("payload is integer");
        assert_eq!(payload, 42);

        let token = output
            .pointer("/authentication/bearer/token")
            .and_then(|v| v.as_str())
            .expect("token");
        assert_eq!(token, "secret-token");
    }
}
