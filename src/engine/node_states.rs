use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use time::OffsetDateTime;

use crate::nodes::id::NodeId;
use crate::types::NodeState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStates(pub HashMap<NodeId, NodeState>);

impl NodeStates {
    pub fn new_for(root: NodeId, raw_input: JsonValue) -> Self {
        let mut map = HashMap::new();
        map.insert(
            root,
            NodeState { started_at: Some(OffsetDateTime::now_utc()), raw_input: Some(raw_input), ..Default::default() },
        );
        Self(map)
    }
}
