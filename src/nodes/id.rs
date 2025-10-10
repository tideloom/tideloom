use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u32);

impl From<usize> for NodeId {
    fn from(value: usize) -> Self { NodeId(value as u32) }
}

impl NodeId {
    pub fn index(self) -> usize { self.0 as usize }
}

