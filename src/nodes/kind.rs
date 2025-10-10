use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FlowKind {
    Root,
    Do,
    For,
    Try,
    Fork,
    Raise,
    Set,
    Switch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EffectKind {
    CallHttp,
    Run,
    Wait,
    Emit,
    Listen,
    CallGrpc,
    CallAsyncApi,
    CallOpenApi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum NodeKind {
    Flow(FlowKind),
    Effect(EffectKind),
}

impl NodeKind {
    pub fn is_effect(&self) -> bool { matches!(self, NodeKind::Effect(_)) }
}

