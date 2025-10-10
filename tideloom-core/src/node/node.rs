use serde::{Deserialize, Serialize};

/// A unique identifier for a node in the workflow graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(u32);

impl NodeId {
    #[inline]
    pub(crate) fn new(id: u32) -> Self {
        Self(id)
    }

    #[inline]
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl From<usize> for NodeId {
    fn from(value: usize) -> Self {
        NodeId(value as u32)
    }
}


/// Flow control node types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlowKind {
    /// Root node of the workflow
    Root,
    /// Sequential execution block
    Do,
    /// Loop/iteration
    For,
    /// Try-catch block
    Try,
    /// Parallel execution
    Fork,
    /// Raise an error
    Raise,
    /// Set a variable
    Set,
    /// Conditional branching
    Switch,
}

/// Effect (side-effect) node types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectKind {
    /// HTTP call
    CallHttp,
    /// Run a task
    Run,
    /// Wait/sleep
    Wait,
    /// Emit an event
    Emit,
    /// Listen for an event
    Listen,
    /// gRPC call
    CallGrpc,
    /// AsyncAPI call
    CallAsyncApi,
    /// OpenAPI call
    CallOpenApi,
}

/// The kind of a node - either a flow control or an effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum NodeKind {
    Flow(FlowKind),
    Effect(EffectKind),
}

impl NodeKind {
    /// Returns true if this is an effect node.
    #[inline]
    pub fn is_effect(&self) -> bool {
        matches!(self, NodeKind::Effect(_))
    }

    /// Returns true if this is a flow control node.
    #[inline]
    pub fn is_flow(&self) -> bool {
        matches!(self, NodeKind::Flow(_))
    }

    /// Returns the inner `EffectKind` if this is an effect node.
    pub fn as_effect(&self) -> Option<&EffectKind> {
        match self {
            NodeKind::Effect(kind) => Some(kind),
            _ => None,
        }
    }

    /// Returns the inner `FlowKind` if this is a flow control node.
    pub fn as_flow(&self) -> Option<&FlowKind> {
        match self {
            NodeKind::Flow(kind) => Some(kind),
            _ => None,
        }
    }
}

/// A runtime instance of a node, containing its metadata and execution state.
/// This struct is used during workflow execution to track the state of each node.
#[derive(Debug, Clone)]
pub struct NodeInstance {
    /// The unique identifier of this node
    pub id: NodeId,
    /// The human-readable name of this node
    pub name: String,
    /// The position of this node in the workflow tree
    /// TODO: replace with NodePosition
    pub position: String,
    /// The execution state of this node
    /// TODO: replace with NodeState
    pub state: String,
}
