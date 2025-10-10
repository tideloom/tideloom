use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkflowId(pub Uuid);

impl WorkflowId {
    pub fn random() -> Self { Self(Uuid::new_v4()) }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct WorkflowName(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct WorkflowVersion(pub String);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeState {
    pub started_at: Option<OffsetDateTime>,
    pub raw_input: Option<JsonValue>,
    pub raw_output: Option<JsonValue>,
    pub child_index: i32,
    pub context: JsonValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    pub workflow_id: WorkflowId,
    pub workflow_name: WorkflowName,
    pub workflow_version: WorkflowVersion,
    pub current_node: crate::nodes::id::NodeId,
    pub current_states: crate::engine::node_states::NodeStates,
}

impl WorkflowState {
    pub fn duplicate(&self, new_id: WorkflowId) -> Self { Self { workflow_id: new_id, ..self.clone() } }
}
