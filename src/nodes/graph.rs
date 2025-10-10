use serde::{Deserialize, Serialize};

use super::{id::NodeId, kind::NodeKind, position::NodePosition};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeGraph {
    kinds: Vec<NodeKind>,
    names: Vec<String>,
    positions: Vec<NodePosition>,
    children: Vec<Vec<NodeId>>,
    parents: Vec<Option<NodeId>>,
    root: NodeId,
}

impl NodeGraph {
    pub fn new_root(name: impl Into<String>) -> Self {
        let name = name.into();
        let root_id = NodeId(0);
        Self {
            kinds: vec![NodeKind::Flow(super::kind::FlowKind::Root)],
            names: vec![name],
            positions: vec![NodePosition::root()],
            children: vec![vec![]],
            parents: vec![None],
            root: root_id,
        }
    }

    pub fn add_node(&mut self, kind: NodeKind, name: impl Into<String>, position: NodePosition) -> NodeId {
        let id = NodeId(self.kinds.len() as u32);
        self.kinds.push(kind);
        self.names.push(name.into());
        self.positions.push(position);
        self.children.push(vec![]);
        self.parents.push(None);
        id
    }

    pub fn add_child(&mut self, parent: NodeId, child: NodeId) {
        self.children[parent.index()].push(child);
        self.parents[child.index()] = Some(parent);
    }

    pub fn root(&self) -> NodeId { self.root }
    pub fn kind(&self, id: NodeId) -> &NodeKind { &self.kinds[id.index()] }
    pub fn name(&self, id: NodeId) -> &str { &self.names[id.index()] }
    pub fn position(&self, id: NodeId) -> &NodePosition { &self.positions[id.index()] }
    pub fn children(&self, id: NodeId) -> &[NodeId] { &self.children[id.index()] }
    pub fn parent(&self, id: NodeId) -> Option<NodeId> { self.parents[id.index()] }
}

