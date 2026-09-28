use std::fmt;

use serde_json::Value;

/// `(namespace, name, version)` from `document`.
///
/// A definition is stored under this triple together with its content hash.
/// Publishing the same name again is a new record, not an edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    namespace: String,
    name: String,
    version: String,
}

impl Identity {
    pub(crate) fn new(namespace: String, name: String, version: String) -> Self {
        Self {
            namespace,
            name,
            version,
        }
    }

    /// Workflow namespace.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Workflow name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Workflow semantic version.
    pub fn version(&self) -> &str {
        &self.version
    }
}

/// Where control goes after a task, or when a switch case matches.
///
/// This is data on the node. The build does not follow it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlowDirective {
    /// Proceed to the next sibling. Also the default when `then` is omitted.
    Continue,
    /// Leave the current `do` scope.
    Exit,
    /// Complete the workflow.
    End,
    /// Jump to a sibling task with this name.
    Task(String),
}

impl FlowDirective {
    pub(crate) fn parse(text: &str) -> Self {
        match text {
            "continue" => Self::Continue,
            "exit" => Self::Exit,
            "end" => Self::End,
            other => Self::Task(other.to_string()),
        }
    }
}

/// What a `call` task invokes.
///
/// `http`, `grpc`, `openapi`, and `asyncapi` are the external calls named in
/// ADR-0001. `a2a` and `mcp` are the other external call constants in DSL
/// 1.0.3, so they are effectful the same way. Any other string is a function
/// call: composite, and inlined when `use.functions` defines it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallKind {
    /// HTTP call.
    Http,
    /// gRPC call.
    Grpc,
    /// OpenAPI call.
    OpenApi,
    /// AsyncAPI call.
    AsyncApi,
    /// Agent-to-agent call.
    A2a,
    /// Model Context Protocol call.
    Mcp,
    /// User function. The string is the `call` value.
    Function(String),
}

impl CallKind {
    pub(crate) fn from_call(call: &str) -> Self {
        match call {
            "http" => Self::Http,
            "grpc" => Self::Grpc,
            "openapi" => Self::OpenApi,
            "asyncapi" => Self::AsyncApi,
            "a2a" => Self::A2a,
            "mcp" => Self::Mcp,
            other => Self::Function(other.to_string()),
        }
    }

    fn is_function(&self) -> bool {
        matches!(self, Self::Function(_))
    }
}

/// What a `run` task starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunKind {
    /// Container image.
    Container,
    /// Shell command.
    Shell,
    /// Script.
    Script,
    /// Another workflow. Blocking and effectful.
    Workflow,
}

/// Task kind recognized while building the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// Synthetic root. Its children are the workflow `do` list.
    Root,
    /// Sequential `do` task.
    Do,
    /// `set` task. Expressions inside the body are not evaluated.
    Set,
    /// `switch` task. Cases are children.
    Switch,
    /// One switch case. `when` is the unevaluated condition, if any.
    SwitchCase {
        /// Unevaluated `when` expression.
        when: Option<String>,
    },
    /// `for` task. The body `do` list is the children.
    For,
    /// `fork` task. `compete` is the DSL flag, defaulting to false.
    Fork {
        /// `fork.compete`. True races branches; false joins all of them.
        compete: bool,
    },
    /// `try` task. `retry` is true when `catch.retry` is set.
    Try {
        /// A retry policy is present, so the task may pause for backoff.
        retry: bool,
    },
    /// `raise` task.
    Raise,
    /// `wait` task.
    Wait,
    /// `listen` task. A `foreach` body, when present, is the children.
    Listen,
    /// `emit` task.
    Emit,
    /// `call` task.
    Call(CallKind),
    /// `run` task.
    Run(RunKind),
}

impl NodeKind {
    /// `(composite, blocking, effectful)` before listen-foreach adjustment.
    fn predicates(&self) -> (bool, bool, bool) {
        match self {
            Self::Root | Self::Do | Self::For | Self::Switch => (true, false, false),
            Self::SwitchCase { .. } | Self::Set | Self::Raise => (false, false, false),
            Self::Fork { .. } => (true, true, false),
            Self::Try { retry } => (true, *retry, false),
            Self::Call(kind) if kind.is_function() => (true, false, false),
            Self::Call(_) => (false, false, true),
            Self::Run(RunKind::Workflow) => (false, true, true),
            Self::Run(_) => (false, false, true),
            Self::Wait | Self::Listen => (false, true, false),
            Self::Emit => (false, false, true),
        }
    }
}

/// One node of the immutable workflow tree.
///
/// Fields are fixed at build time. There is no setter. `composite`,
/// `blocking`, and `effectful` are independent; callers look them up instead
/// of classifying the task again.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    position: String,
    name: Option<String>,
    kind: NodeKind,
    composite: bool,
    blocking: bool,
    effectful: bool,
    then: FlowDirective,
    children: Vec<Node>,
    body: Value,
}

impl Node {
    pub(crate) fn new(
        position: String,
        name: Option<String>,
        kind: NodeKind,
        then: FlowDirective,
        children: Vec<Node>,
        body: Value,
    ) -> Self {
        let (mut composite, blocking, effectful) = kind.predicates();
        if matches!(kind, NodeKind::Listen) && !children.is_empty() {
            composite = true;
        }
        Self {
            position,
            name,
            kind,
            composite,
            blocking,
            effectful,
            then,
            children,
            body,
        }
    }

    /// JSON Pointer of this node.
    ///
    /// The root pointer is empty. Task pointers address the task inside the
    /// document, for example `/do/0/validateOrder`. A function inlined from
    /// `use.functions` lives at `{call}/_fn`, which is not a pointer into the
    /// original `call` object.
    pub fn position(&self) -> &str {
        &self.position
    }

    /// Task name, function name, or switch-case name.
    ///
    /// The root uses the workflow name.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Recognized kind.
    pub fn kind(&self) -> &NodeKind {
        &self.kind
    }

    /// The node has children the interpreter navigates into.
    pub fn composite(&self) -> bool {
        self.composite
    }

    /// The node may pause (`wake_at` or a wait condition).
    pub fn blocking(&self) -> bool {
        self.blocking
    }

    /// The node has an external side effect and would record a `task_execution` row.
    pub fn effectful(&self) -> bool {
        self.effectful
    }

    /// Block boundary: `blocking` or `effectful`.
    pub fn is_block_boundary(&self) -> bool {
        self.blocking || self.effectful
    }

    /// Runner routing and a `task_execution` row apply only to effectful nodes.
    pub fn records_task_execution(&self) -> bool {
        self.effectful
    }

    /// Unevaluated flow directive. Omitted `then` is [`FlowDirective::Continue`].
    pub fn then(&self) -> &FlowDirective {
        &self.then
    }

    /// Child nodes, in document order.
    ///
    /// A `try` lists the `try` tasks first and then the `catch.do` tasks.
    /// Positions distinguish the two bodies.
    pub fn children(&self) -> &[Node] {
        &self.children
    }

    /// Unevaluated source object for this node.
    ///
    /// Runtime expressions are stored here as text or JSON. This build does
    /// not evaluate them.
    pub fn body(&self) -> &Value {
        &self.body
    }

    /// This node, then each descendant, parent before children.
    pub fn walk(&self) -> impl Iterator<Item = &Node> {
        Walk { stack: vec![self] }
    }
}

struct Walk<'a> {
    stack: Vec<&'a Node>,
}

impl<'a> Iterator for Walk<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        for child in node.children.iter().rev() {
            self.stack.push(child);
        }
        Some(node)
    }
}

impl fmt::Display for FlowDirective {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Continue => formatter.write_str("continue"),
            Self::Exit => formatter.write_str("exit"),
            Self::End => formatter.write_str("end"),
            Self::Task(name) => formatter.write_str(name),
        }
    }
}
