use serde_json::Value as JsonValue;

use crate::activities::{executor::EffectContext, registry::EffectRegistry};
use crate::errors::WorkflowError;
use crate::messaging::Message;
use crate::nodes::{graph::NodeGraph, id::NodeId, instance::{NodeInstance, SimpleInstance}, kind::NodeKind};
use crate::outbox::{Backoff, OutboxItem};
use crate::types::{WorkflowName, WorkflowState, WorkflowVersion};

type TaskStarted = Box<dyn FnMut(&dyn NodeInstance)>;
type TaskCompleted = Box<dyn FnMut(&dyn NodeInstance)>;
type TaskFaulted = Box<dyn FnMut(&dyn NodeInstance)>;
type WorkflowStarted = Box<dyn FnMut()>;
type WorkflowCompleted = Box<dyn FnMut()>;

pub struct Processor {
    pub workflow_state: WorkflowState,
    pub effects: EffectRegistry,

    pub status: Status,
    pub graph: NodeGraph,
    pub current: Option<NodeId>,

    on_workflow_started: WorkflowStarted,
    on_workflow_completed: WorkflowCompleted,
    on_task_started: TaskStarted,
    on_task_completed: TaskCompleted,
    on_task_faulted: TaskFaulted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status { Pending, Running, Waiting, Completed, Faulted }

#[derive(Debug, Clone)]
pub enum Step {
    Next(NodeId),
    Wait(OutboxItem),
    Emit(Message),
    Retry(Backoff),
    Done(JsonValue),
    Fault(WorkflowError),
}

impl Processor {
    pub fn new(workflow_state: WorkflowState, effects: EffectRegistry, graph: NodeGraph) -> Self {
        Self {
            current: Some(workflow_state.current_node),
            workflow_state,
            effects,
            status: Status::Pending,
            graph,
            on_workflow_started: Box::new(|| {}),
            on_workflow_completed: Box::new(|| {}),
            on_task_started: Box::new(|_| {}),
            on_task_completed: Box::new(|_| {}),
            on_task_faulted: Box::new(|_| {}),
        }
    }

    pub fn on_workflow_started(&mut self, f: WorkflowStarted) { self.on_workflow_started = f; }
    pub fn on_workflow_completed(&mut self, f: WorkflowCompleted) { self.on_workflow_completed = f; }
    pub fn on_task_started(&mut self, f: TaskStarted) { self.on_task_started = f; }
    pub fn on_task_completed(&mut self, f: TaskCompleted) { self.on_task_completed = f; }
    pub fn on_task_faulted(&mut self, f: TaskFaulted) { self.on_task_faulted = f; }

    pub async fn tick_async(&mut self) -> Step {
        if matches!(self.status, Status::Pending) {
            self.status = Status::Running;
            (self.on_workflow_started.as_mut())();
        }

        let Some(id) = self.current.clone() else {
            self.status = Status::Completed;
            (self.on_workflow_completed.as_mut())();
            return Step::Done(self.output());
        };

        let kind = self.graph.kind(id).clone();

        {
            let state = self
                .workflow_state
                .current_states
                .0
                .entry(id)
                .or_default();
            if state.started_at.is_none() {
                let inst = SimpleInstance::new(id, self.graph.name(id).to_string(), self.graph.position(id).clone());
                (self.on_task_started.as_mut())(&inst);
            }
        }

        match kind {
            NodeKind::Effect(eff) => {
                let ctx = EffectContext { id, name: self.graph.name(id).to_string(), kind: eff };
                let output = self.effects.execute(&ctx).await.unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
                {
                    let state = self
                        .workflow_state
                        .current_states
                        .0
                        .entry(id)
                        .or_default();
                    state.raw_output = Some(output);
                }
                let inst = SimpleInstance::new(id, self.graph.name(id).to_string(), self.graph.position(id).clone());
                (self.on_task_completed.as_mut())(&inst);

                match self.graph.parent(id) {
                    Some(parent_id) => {
                        let parent_state = self
                            .workflow_state
                            .current_states
                            .0
                            .entry(parent_id)
                            .or_default();
                        parent_state.child_index += 1;
                        self.current = Some(parent_id);
                        Step::Next(parent_id)
                    }
                    None => {
                        self.current = None;
                        Step::Done(self.output())
                    }
                }
            }
            NodeKind::Flow(_) => {
                let next_index = {
                    let s = self
                        .workflow_state
                        .current_states
                        .0
                        .entry(id)
                        .or_default();
                    s.child_index as usize
                };
                let children = self.graph.children(id);
                if next_index < children.len() {
                    let child = children[next_index];
                    self.current = Some(child);
                    Step::Next(child)
                } else {
                    if let Some(&last_child) = children.last() {
                        let child_output = self
                            .workflow_state
                            .current_states
                            .0
                            .get(&last_child)
                            .and_then(|s| s.raw_output.clone());
                        if let Some(out) = child_output {
                            let s = self
                                .workflow_state
                                .current_states
                                .0
                                .entry(id)
                                .or_default();
                            s.raw_output = Some(out);
                        }
                    }
                    let inst = SimpleInstance::new(id, self.graph.name(id).to_string(), self.graph.position(id).clone());
                    (self.on_task_completed.as_mut())(&inst);
                    match self.graph.parent(id) {
                        Some(parent_id) => {
                            let parent_state = self
                                .workflow_state
                                .current_states
                                .0
                                .entry(parent_id)
                                .or_default();
                            parent_state.child_index += 1;
                            self.current = Some(parent_id);
                            Step::Next(parent_id)
                        }
                        None => {
                            self.current = None;
                            Step::Done(self.output())
                        }
                    }
                }
            }
        }
    }

    pub async fn run_async(&mut self) -> JsonValue {
        loop {
            match self.tick_async().await {
                Step::Done(v) => break v,
                Step::Fault(_) => break self.output(),
                _ => continue,
            }
        }
    }

    pub fn output(&self) -> JsonValue {
        self.workflow_state
            .current_states
            .0
            .get(&self.graph.root())
            .and_then(|s| s.raw_output.clone())
            .unwrap_or_else(|| JsonValue::Null)
    }

    pub fn workflow_name(&self) -> &WorkflowName { &self.workflow_state.workflow_name }
    pub fn workflow_version(&self) -> &WorkflowVersion { &self.workflow_state.workflow_version }
}
