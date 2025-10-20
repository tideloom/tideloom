use serde::Deserialize;
use serde_json::{Map, Value};
use serverless_workflow_core::models::authentication::AuthenticationPolicyDefinition;
use serverless_workflow_core::models::task::{CallTaskDefinition, TaskDefinition};
use std::str::FromStr;

use crate::runtime::{Task, StepResult, WorkflowContext};

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
pub struct HTTPNode {
    endpoint: reqwest::Url,
    method: reqwest::Method,
}

impl HTTPNode {
    pub fn try_from_task(task: &TaskDefinition) -> StepResult<Self> {
        match task {
            TaskDefinition::Call(call) => match call.call.to_lowercase().as_str() {
                "asyncapi" => Self::try_from_http(call),
                "http" => Self::try_from_http(call),
                _ => Err(format!("expected call 'asyncapi', got '{}'", call.call)),
            },
            _ => Err("AsyncApiNode expects a `call` task definition".into()),
        }
    }

    pub fn try_from_http(call: &CallTaskDefinition) -> StepResult<Self> {
        let with = call
            .with
            .as_ref()
            .ok_or_else(|| "asyncapi call requires a `with` block".to_string())?;

        let mut with_map = Map::new();
        for (key, value) in with {
            with_map.insert(key.clone(), value.clone());
        }

        let endpoint_url = with_map.get("endpoint").unwrap().as_str().unwrap();
        let method = with_map.get("method").unwrap().as_str().unwrap().to_string();

        let config: HTTPNode = HTTPNode {
            endpoint: reqwest::Url::parse(endpoint_url).unwrap(),
            method: reqwest::Method::from_str(&method).unwrap(),
        };

        Ok(config)
    }

    fn build_request(&self, _input: &Value) -> reqwest::Request {
        

        reqwest::Request::new(
            self.method.clone(),
            self.endpoint.clone(),
        )
    }
}

impl TryFrom<&TaskDefinition> for HTTPNode {
    type Error = String;

    fn try_from(task: &TaskDefinition) -> std::result::Result<Self, Self::Error> {
        Self::try_from_task(task)
    }
}

impl TryFrom<&CallTaskDefinition> for HTTPNode {
    type Error = String;

    fn try_from(call: &CallTaskDefinition) -> std::result::Result<Self, Self::Error> {
        Self::try_from_http(call)
    }
}

#[async_trait::async_trait]
impl Task for HTTPNode {
    type Input = Value;
    type Output = Value;

    async fn execute(&self, ctx: &WorkflowContext, input: Self::Input) -> StepResult<Self::Output> {
        let req = self.build_request(&input);
        ctx.http_client.execute(req).await.unwrap();

        // TODO: fix me
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
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
    async fn http_node_from_task() {
        let yaml = r#"
document:
  dsl: '1.0.1'
  namespace: test
  name: http-example
  version: '0.1.0'
do:
 - test:
     call: http
     with:
        method: get
        endpoint: https://httpbin.org/get

 "#;

        let task = load_first_task(yaml);
        let step = HTTPNode::try_from_task(&task).expect("asyncapi node");
        let c = WorkflowContext::default();
        let ctx = WorkflowContext::default();
        let input = json!({});

        let output = step
            .execute(&ctx, input)
            .await
            .expect("step should succeed");

        println!("{:?}", output);
    }
}
