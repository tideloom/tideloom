use time::OffsetDateTime;

use super::{id::NodeId, position::NodePosition};
use crate::types::NodeState;

pub trait NodeInstance {
    fn id(&self) -> NodeId;
    fn name(&self) -> &str;
    fn position(&self) -> &NodePosition;
    fn state(&self) -> &NodeState;
    fn state_mut(&mut self) -> &mut NodeState;

    fn should_start(&self) -> bool { true }
}

pub struct SimpleInstance {
    pub id: NodeId,
    pub name: String,
    pub position: NodePosition,
    pub state: NodeState,
}

impl SimpleInstance {
    pub fn new(id: NodeId, name: impl Into<String>, position: NodePosition) -> Self {
        Self { id, name: name.into(), position, state: NodeState { started_at: Some(OffsetDateTime::now_utc()), ..Default::default() } }
    }
}

impl NodeInstance for SimpleInstance {
    fn id(&self) -> NodeId { self.id }
    fn name(&self) -> &str { &self.name }
    fn position(&self) -> &NodePosition { &self.position }
    fn state(&self) -> &NodeState { &self.state }
    fn state_mut(&mut self) -> &mut NodeState { &mut self.state }
}
