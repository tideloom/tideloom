use std::sync::Arc;

use serde_json::json;
use tideloom::activities::{executor::SimpleRunExecutor, registry::EffectRegistry};
use tideloom::engine::{NodeStates, Processor};
use tideloom::nodes::{graph::NodeGraph, kind::{EffectKind, NodeKind}, position::NodePosition};
use tideloom::types::{WorkflowId, WorkflowName, WorkflowState, WorkflowVersion};

#[tokio::main]
async fn main() {
    // Build a simple graph: root(flow) -> run(effect)
    let mut graph = NodeGraph::new_root("root");
    let run_id = graph.add_node(NodeKind::Effect(EffectKind::Run), "run", NodePosition::root().add_name("run"));
    graph.add_child(graph.root(), run_id);

    // State
    let state = WorkflowState {
        workflow_id: WorkflowId::random(),
        workflow_name: WorkflowName("example".into()),
        workflow_version: WorkflowVersion("0.1.0".into()),
        current_node: graph.root(),
        current_states: NodeStates::new_for(graph.root(), json!({ "input": true })),
    };

    // Effects registry
    let registry = EffectRegistry::new().with_executor(Arc::new(SimpleRunExecutor));

    // Processor
    let mut processor = Processor::new(state, registry, graph);
    let output = processor.run_async().await;
    println!("{}", output);
}
