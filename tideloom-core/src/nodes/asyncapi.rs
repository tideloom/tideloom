use serde::Deserialize;
use serde_json::{Map, Value, json};
use serverless_workflow_core::models::authentication::AuthenticationPolicyDefinition;
use serverless_workflow_core::models::task::{CallTaskDefinition, TaskDefinition};

use crate::runtime::{StepResult, Step, WorkflowContext};

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
pub struct AsyncApiNode {
    config: AsyncApiConfig,
}

impl AsyncApiNode {
    pub fn try_from_task(task: &TaskDefinition) -> StepResult<Self> {
        match task {
            TaskDefinition::Call(call) => Self::try_from_call(call),
            _ => Err("AsyncApiNode expects a `call` task definition".into()),
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

    fn build_request(&self, input: &Value) -> reqwest::Request {
        let mut request = reqwest::Request::new(reqwest::Method::GET, Url::parse("https://example.com").unwrap());

        request
    }
}

impl TryFrom<&TaskDefinition> for AsyncApiNode {
    type Error = String;

    fn try_from(task: &TaskDefinition) -> std::result::Result<Self, Self::Error> {
        Self::try_from_task(task)
    }
}

impl TryFrom<&CallTaskDefinition> for AsyncApiNode {
    type Error = String;

    fn try_from(call: &CallTaskDefinition) -> std::result::Result<Self, Self::Error> {
        Self::try_from_call(call)
    }
}

impl Step for AsyncApiNode {
    type Input = Value;
    type Output = Value;

    async fn execute(
        &self,
        ctx: &WorkflowContext,
        input: Self::Input,
    ) -> StepResult<Self::Output> {
        let req = self.build_request(&input);
        ctx.http_client.execute(req).await?;
    }
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
        let step = AsyncApiNode::try_from_task(&task).expect("asyncapi node");
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
