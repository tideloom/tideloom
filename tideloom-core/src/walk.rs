//! Re-walk an immutable definition from the root.
//!
//! Resume state is a [`ResultLog`](crate::ResultLog), not a node stack. The
//! frames used to build an execution key exist only for the duration of the
//! call. Control-flow tasks run inline. The walk stops at the next block
//! boundary the log cannot satisfy and returns that block for a runner.
//!
//! Expressions evaluated here are a closed subset: `.path` and `$name`,
//! literals, comparisons, `+ - * /`, `and` / `or` / `not`, parentheses, arrays,
//! objects, and `|`. A `set` string is literal text unless it contains
//! `${ ... }`. Full jq is not implemented; an unsupported expression is an
//! expression fault.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Map;
use serde_json::Value;

use crate::CallKind;
use crate::Definition;
use crate::Fault;
use crate::FlowDirective;
use crate::JitterSample;
use crate::Node;
use crate::NodeKind;
use crate::ResultLog;
use crate::RunKind;
use crate::TaskKey;
use crate::Timestamp;
use crate::expr::evaluate;
use crate::expr::evaluate_data;
use crate::expr::runtime_expression;
use crate::log::ExecutionKey;
use crate::log::Frame;
use crate::retry::duration_millis;
use crate::retry::retry_spec;
use crate::retry::wait_ms;

/// Stop an inline loop that never reaches a block boundary.
const MAX_INLINE_STEPS: u32 = 10_000;

/// Why a [`Block`] was handed to a runner.
///
/// These are the ADR-0001 pause reasons. [`Pause::Activity`] and
/// [`Pause::Child`] are effectful and record a `task_execution`. The others
/// are blocking only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pause {
    /// Effectful work other than `run workflow`: `call`, `emit`, `run` shell,
    /// script, or container.
    Activity,
    /// `wait`.
    Timer,
    /// `listen`. The `foreach` body is not walked in this slice.
    Events,
    /// `fork`. Branches are not walked; they belong to other instances.
    Join {
        /// `fork.compete`.
        compete: bool,
    },
    /// `try` retry backoff. `attempt` is the attempt that just failed.
    /// `delay` is the wait before the next attempt, including backoff and jitter.
    Retry {
        /// Zero-based attempt that failed.
        attempt: u64,
        /// Wait before the next attempt. Add it to the instant this pause was
        /// first observed. See [`Pause::retry_at`].
        delay: Duration,
    },
    /// `run workflow`. Also effectful.
    Child,
}

/// One block a runner would claim.
///
/// The walk ran every non-boundary task before this node. `input` is the
/// transformed task input. `context` is `$context` before this task's
/// `export.as`.
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    key: TaskKey,
    name: Option<String>,
    pause: Pause,
    input: Value,
    context: Value,
    effectful: bool,
    kind: NodeKind,
    timeout: Option<Duration>,
}

impl Block {
    /// JSON Pointer plus execution key. Use this as the result-log key.
    pub fn key(&self) -> &TaskKey {
        &self.key
    }

    /// JSON Pointer of the boundary node.
    pub fn position(&self) -> &str {
        self.key.position()
    }

    /// Ancestor `for` indexes and `try` attempts, outermost first.
    pub fn execution_key(&self) -> &ExecutionKey {
        self.key.execution_key()
    }

    /// Task name.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Pause reason.
    pub fn pause(&self) -> Pause {
        self.pause
    }

    /// Transformed input the runner should use.
    pub fn input(&self) -> &Value {
        &self.input
    }

    /// `$context` at the stop.
    pub fn context(&self) -> &Value {
        &self.context
    }

    /// Whether the runner records a `task_execution` for this block.
    pub fn effectful(&self) -> bool {
        self.effectful
    }

    /// Node kind, so a runner can route without another tree lookup.
    pub fn kind(&self) -> &NodeKind {
        &self.kind
    }

    /// Task `timeout.after`, when the task sets one.
    #[must_use]
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Instant the task times out: `started_at + timeout.after`.
    ///
    /// `None` when the task has no timeout. The runner keeps `started_at` from
    /// [`crate::ResultLog::start_task`]. The walk does not sleep.
    #[must_use]
    pub fn timeout_at(&self, started_at: Timestamp) -> Option<Timestamp> {
        self.timeout.map(|after| started_at.saturating_add(after))
    }
}

impl Pause {
    /// Instant a retry pause may be released: `started_at + delay`.
    ///
    /// `started_at` is when the runner first observed this pause. A later walk
    /// returns the same delay for the same attempt and jitter sample; it does
    /// not move `started_at`, and it does not sleep. Other pause reasons
    /// return `None`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use tideloom_core::Pause;
    /// use tideloom_core::Timestamp;
    ///
    /// let pause = Pause::Retry {
    ///     attempt: 0,
    ///     delay: Duration::from_secs(3),
    /// };
    /// let started = Timestamp::from_millis(1_000);
    /// assert_eq!(pause.retry_at(started), Some(Timestamp::from_millis(4_000)));
    /// assert_eq!(Pause::Timer.retry_at(started), None);
    /// ```
    #[must_use]
    pub fn retry_at(&self, started_at: Timestamp) -> Option<Timestamp> {
        match self {
            Self::Retry { delay, .. } => Some(started_at.saturating_add(*delay)),
            Self::Activity | Self::Timer | Self::Events | Self::Join { .. } | Self::Child => None,
        }
    }
}

/// Knobs that are not the definition or the result log.
///
/// [`walk`] uses [`WalkOptions::default`], which pins jitter to `jitter.from`.
/// Pass the same options on every re-walk so the computed delay stays put.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalkOptions {
    jitter: JitterSample,
    now: Option<Timestamp>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        Self {
            jitter: JitterSample::FROM,
            now: None,
        }
    }
}

impl WalkOptions {
    /// Jitter sample at the `from` end of the range.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pick a point in `jitter.from` ..= `jitter.to`.
    ///
    /// [`JitterSample::FROM`] is `from`. [`JitterSample::TO`] is `to`.
    #[must_use]
    pub fn with_jitter(mut self, sample: JitterSample) -> Self {
        self.jitter = sample;
        self
    }

    /// Clock reading for workflow and task timeouts.
    ///
    /// Without it, a configured timeout is reported on the block and is not
    /// enforced. The same `now` on a later walk, with a start instant already
    /// stored in the log, faults once `now` reaches the deadline.
    #[must_use]
    pub fn with_now(mut self, now: Timestamp) -> Self {
        self.now = Some(now);
        self
    }
}

/// What one walk from the root decided.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The workflow finished. `output` has workflow `output.as` applied.
    Completed {
        /// Workflow output.
        output: Value,
        /// `$context` after every export on this walk.
        context: Value,
    },
    /// An uncaught fault, including an expression the subset cannot evaluate.
    Faulted {
        /// Problem details.
        fault: Fault,
        /// `$context` after exports that completed before the fault.
        context: Value,
    },
    /// The next block boundary. Inline work before it has already run.
    Blocked {
        /// The unit a runner receives.
        block: Block,
    },
}

/// Re-walk `definition` from the root.
///
/// `input` is the workflow input. `log` supplies effectful outputs and
/// released blocking pauses. The same arguments produce the same outcome.
/// An inline cycle that never hits a boundary faults after 10_000 task entries.
///
/// A `try` retry pause carries the computed delay. This function does not
/// sleep. [`Pause::retry_at`] adds that delay to the instant the runner first
/// saw the pause. [`crate::ResultLog::release`] on the pause key makes the
/// next walk retry the try at the next attempt. Jitter uses `jitter.from`.
/// [`walk_with`] selects another point in the jitter range and supplies `now`
/// for workflow and task timeouts.
///
/// # Examples
///
/// ```
/// use tideloom_core::Definition;
/// use tideloom_core::Outcome;
/// use tideloom_core::Pause;
/// use tideloom_core::ResultLog;
/// use tideloom_core::walk;
///
/// let yaml = r#"
/// document:
///   dsl: '1.0.0'
///   namespace: default
///   name: paint
///   version: '0.1.0'
/// do:
///   - setColor:
///       set:
///         color: red
///   - notify:
///       emit:
///         event:
///           with:
///             source: https://example.test/paint
///             type: io.example.painted
/// "#;
/// let definition = Definition::from_yaml(yaml).unwrap();
/// let outcome = walk(&definition, &serde_json::json!({}), &ResultLog::new());
/// let Outcome::Blocked { block } = outcome else {
///     panic!("expected a block");
/// };
/// assert_eq!(block.position(), "/do/1/notify");
/// assert!(block.effectful());
/// assert!(matches!(block.pause(), Pause::Activity));
/// assert_eq!(block.input()["color"], "red");
/// ```
#[must_use]
pub fn walk(definition: &Definition, input: &Value, log: &ResultLog) -> Outcome {
    walk_with(definition, input, log, WalkOptions::default())
}

/// [`walk`] with an explicit jitter sample and clock reading.
///
/// The sample is part of the pure inputs. A later call with the same sample,
/// definition, input, and log returns the same retry delay. `now` enforces
/// workflow and task timeouts against start instants stored in the log.
#[must_use]
pub fn walk_with(
    definition: &Definition,
    input: &Value,
    log: &ResultLog,
    options: WalkOptions,
) -> Outcome {
    let mut interpreter = Interpreter {
        log,
        document: definition.root().body(),
        workflow_input: input.clone(),
        context: Map::new(),
        frames: Vec::new(),
        bindings: Vec::new(),
        steps: 0,
        jitter: options.jitter,
        now: options.now,
    };
    match interpreter.workflow_timeout(input) {
        Ok(None) => {}
        Ok(Some(fault)) | Err(fault) => {
            return Outcome::Faulted {
                fault,
                context: Value::Object(Map::new()),
            };
        }
    }
    let started = match interpreter.workflow_input(input) {
        Ok(value) => value,
        Err(fault) => {
            return Outcome::Faulted {
                fault,
                context: Value::Object(Map::new()),
            };
        }
    };
    match interpreter.exec_sequence(definition.root().children(), started) {
        Ok(Flow::Blocked(block)) => Outcome::Blocked { block },
        Ok(flow) => {
            let raw = match flow {
                Flow::Finished(value) | Flow::Exited(value) | Flow::Ended(value) => value,
                Flow::Blocked(_) => unreachable!("blocked outcome returned above"),
            };
            let context = Value::Object(interpreter.context.clone());
            match interpreter.workflow_output(raw) {
                Ok(output) => Outcome::Completed { output, context },
                Err(fault) => Outcome::Faulted { fault, context },
            }
        }
        Err(fault) => Outcome::Faulted {
            fault,
            context: Value::Object(interpreter.context),
        },
    }
}

struct Interpreter<'a> {
    log: &'a ResultLog,
    document: &'a Value,
    workflow_input: Value,
    context: Map<String, Value>,
    frames: Vec<Frame>,
    bindings: Vec<BTreeMap<String, Value>>,
    steps: u32,
    jitter: JitterSample,
    now: Option<Timestamp>,
}

enum Directive {
    Continue,
    Exit,
    End,
    Jump(String),
}

enum TaskFlow {
    Done { output: Value, directive: Directive },
    Blocked(Block),
}

enum Flow {
    Finished(Value),
    Exited(Value),
    Ended(Value),
    Blocked(Block),
}

impl<'a> Interpreter<'a> {
    fn workflow_input(&self, input: &Value) -> Result<Value, Fault> {
        match optional_string(self.document, &["input", "from"], "/")? {
            Some(source) => {
                let vars = self.scope(None, input, None);
                evaluate(source, input, &vars).map_err(|message| Fault::expression("/", message))
            }
            None => Ok(input.clone()),
        }
    }

    fn workflow_output(&self, output: Value) -> Result<Value, Fault> {
        match optional_string(self.document, &["output", "as"], "/")? {
            Some(source) => {
                let vars = self.scope(None, &self.workflow_input, Some(&output));
                evaluate(source, &output, &vars).map_err(|message| Fault::expression("/", message))
            }
            None => Ok(output),
        }
    }

    fn exec_sequence(&mut self, children: &[Node], mut data: Value) -> Result<Flow, Fault> {
        if children.is_empty() {
            return Ok(Flow::Finished(data));
        }
        let mut index = 0;
        while index < children.len() {
            match self.exec_task(&children[index], data)? {
                TaskFlow::Blocked(block) => return Ok(Flow::Blocked(block)),
                TaskFlow::Done { output, directive } => {
                    data = output;
                    match directive {
                        Directive::Continue => index += 1,
                        Directive::Exit => return Ok(Flow::Exited(data)),
                        Directive::End => return Ok(Flow::Ended(data)),
                        Directive::Jump(name) => {
                            let from = children[index].position();
                            index = children
                                .iter()
                                .position(|child| child.name() == Some(name.as_str()))
                                .ok_or_else(|| {
                                    Fault::runtime(from, format!("no sibling task `{name}`"))
                                })?;
                        }
                    }
                }
            }
        }
        Ok(Flow::Finished(data))
    }

    fn exec_task(&mut self, node: &Node, raw_input: Value) -> Result<TaskFlow, Fault> {
        self.charge(node.position())?;
        if let Some(source) = optional_string(node.body(), &["if"], node.position())? {
            let vars = self.scope(Some(node), &raw_input, None);
            if !self.eval_bool(source, &raw_input, &vars, node.position())? {
                return Ok(TaskFlow::Done {
                    output: raw_input,
                    directive: directive_of(node.then()),
                });
            }
        }
        let input = if let Some(source) =
            optional_string(node.body(), &["input", "from"], node.position())?
        {
            let vars = self.scope(Some(node), &raw_input, None);
            evaluate(source, &raw_input, &vars)
                .map_err(|message| Fault::expression(node.position(), message))?
        } else {
            raw_input
        };
        self.dispatch(node, input)
    }

    fn dispatch(&mut self, node: &Node, input: Value) -> Result<TaskFlow, Fault> {
        match node.kind() {
            NodeKind::Set => self.exec_set(node, &input),
            NodeKind::Raise => Err(self.raise_fault(node)?),
            NodeKind::Switch => self.exec_switch(node, &input),
            NodeKind::Do => self.exec_do(node, &input),
            NodeKind::For => self.exec_for(node, &input),
            NodeKind::Try { .. } => self.exec_try(node, &input),
            NodeKind::Fork { .. } | NodeKind::Wait | NodeKind::Listen => {
                self.exec_pause(node, &input)
            }
            NodeKind::Call(CallKind::Function(_)) => self.exec_function(node, &input),
            NodeKind::Call(_) | NodeKind::Emit | NodeKind::Run(_) => {
                self.exec_effectful(node, &input)
            }
            NodeKind::Root | NodeKind::SwitchCase { .. } => Err(Fault::runtime(
                node.position(),
                "internal node is not a task",
            )),
        }
    }

    fn exec_set(&mut self, node: &Node, input: &Value) -> Result<TaskFlow, Fault> {
        let Some(set) = node.body().get("set") else {
            return Err(Fault::runtime(node.position(), "set task is missing `set`"));
        };
        let vars = self.scope(Some(node), input, None);
        let assigned = evaluate_data(set, input, &vars)
            .map_err(|message| Fault::expression(node.position(), message))?;
        let Value::Object(assigned) = assigned else {
            return Err(Fault::expression(
                node.position(),
                "`set` must evaluate to an object",
            ));
        };
        self.done(
            node,
            merge_object(input, assigned),
            input,
            directive_of(node.then()),
        )
    }

    fn exec_switch(&mut self, node: &Node, input: &Value) -> Result<TaskFlow, Fault> {
        let vars = self.scope(Some(node), input, None);
        for case in node.children() {
            let matched = match case.kind() {
                NodeKind::SwitchCase { when: None } => true,
                NodeKind::SwitchCase { when: Some(when) } => {
                    self.eval_bool(when, input, &vars, case.position())?
                }
                _ => false,
            };
            if matched {
                return self.done(node, input.clone(), input, directive_of(case.then()));
            }
        }
        Err(Fault::expression(node.position(), "no switch case matched"))
    }

    fn exec_do(&mut self, node: &Node, input: &Value) -> Result<TaskFlow, Fault> {
        match self.exec_sequence(node.children(), input.clone())? {
            Flow::Finished(raw) | Flow::Exited(raw) => {
                self.done(node, raw, input, directive_of(node.then()))
            }
            Flow::Ended(raw) => self.done(node, raw, input, Directive::End),
            Flow::Blocked(block) => Ok(TaskFlow::Blocked(block)),
        }
    }

    fn exec_for(&mut self, node: &Node, input: &Value) -> Result<TaskFlow, Fault> {
        let source = optional_string(node.body(), &["for", "in"], node.position())?
            .ok_or_else(|| Fault::runtime(node.position(), "for.in is missing"))?;
        let vars = self.scope(Some(node), input, None);
        let collection = evaluate(source, input, &vars)
            .map_err(|message| Fault::expression(node.position(), message))?;
        let Value::Array(items) = collection else {
            return Err(Fault::expression(
                node.position(),
                "for.in must be an array",
            ));
        };
        let each = var_name(node, &["for", "each"], "item")?;
        let at = var_name(node, &["for", "at"], "index")?;
        if each == at {
            return Err(Fault::runtime(
                node.position(),
                "for.each and for.at must be different names",
            ));
        }
        let while_expr = optional_string(node.body(), &["while"], node.position())?;
        let mut data = input.clone();
        for (index, item) in items.iter().enumerate() {
            let index_u64 = u64::try_from(index)
                .map_err(|_| Fault::runtime(node.position(), "for index does not fit in u64"))?;
            let mut binding = BTreeMap::new();
            binding.insert(each.clone(), item.clone());
            binding.insert(at.clone(), Value::from(index_u64));
            self.bindings.push(binding);
            let proceed = if let Some(source) = while_expr {
                let vars = self.scope(Some(node), input, None);
                self.eval_bool(source, &data, &vars, node.position())
            } else {
                Ok(true)
            };
            let proceed = match proceed {
                Ok(value) => value,
                Err(fault) => {
                    self.bindings.pop();
                    return Err(fault);
                }
            };
            if !proceed {
                self.bindings.pop();
                break;
            }
            self.frames.push(Frame::Loop(index_u64));
            let sequence = self.exec_sequence(node.children(), data);
            self.frames.pop();
            self.bindings.pop();
            match sequence? {
                Flow::Finished(output) => data = output,
                Flow::Exited(output) => {
                    data = output;
                    break;
                }
                Flow::Ended(output) => return self.done(node, output, input, Directive::End),
                Flow::Blocked(block) => return Ok(TaskFlow::Blocked(block)),
            }
        }
        self.done(node, data, input, directive_of(node.then()))
    }

    fn exec_try(&mut self, node: &Node, input: &Value) -> Result<TaskFlow, Fault> {
        let max_retries = self.retry_limit(node)?;
        let catch_at = node
            .children()
            .iter()
            .position(|child| {
                child
                    .position()
                    .starts_with(&format!("{}/catch/", node.position()))
            })
            .unwrap_or(node.children().len());
        let (try_body, catch_body) = node.children().split_at(catch_at);
        let mut attempt = 0u64;
        loop {
            self.frames.push(Frame::Attempt(attempt));
            let sequence = self.exec_sequence(try_body, input.clone());
            self.frames.pop();
            match sequence {
                Ok(Flow::Blocked(block)) => return Ok(TaskFlow::Blocked(block)),
                Ok(Flow::Finished(raw) | Flow::Exited(raw)) => {
                    return self.done(node, raw, input, directive_of(node.then()));
                }
                Ok(Flow::Ended(raw)) => return self.done(node, raw, input, Directive::End),
                Err(fault) => {
                    let error_name = error_binding(node)?;
                    let mut binding = BTreeMap::new();
                    binding.insert(error_name, fault.to_value());
                    self.bindings.push(binding);
                    let matched = self.catch_matches(node, &fault, input);
                    let matched = match matched {
                        Ok(value) => value,
                        Err(error) => {
                            self.bindings.pop();
                            return Err(error);
                        }
                    };
                    if !matched {
                        self.bindings.pop();
                        return Err(fault);
                    }
                    let retry = match self.retry_wanted(node, input, attempt, max_retries) {
                        Ok(value) => value,
                        Err(error) => {
                            self.bindings.pop();
                            return Err(error);
                        }
                    };
                    if retry {
                        let key = self.key_for_attempt(node, attempt);
                        if self.log.released(&key).is_some() {
                            self.bindings.pop();
                            attempt += 1;
                            continue;
                        }
                        let paused = self.retry_block(node, input, attempt);
                        self.bindings.pop();
                        return paused;
                    }
                    if catch_body.is_empty() {
                        self.bindings.pop();
                        return Err(fault);
                    }
                    self.frames.push(Frame::Attempt(attempt));
                    let catch_sequence = self.exec_sequence(catch_body, input.clone());
                    self.frames.pop();
                    self.bindings.pop();
                    let directive = catch_directive(node);
                    return match catch_sequence? {
                        Flow::Finished(raw) | Flow::Exited(raw) => {
                            self.done(node, raw, input, directive)
                        }
                        Flow::Ended(raw) => self.done(node, raw, input, Directive::End),
                        Flow::Blocked(block) => Ok(TaskFlow::Blocked(block)),
                    };
                }
            }
        }
    }

    fn exec_function(&mut self, node: &Node, input: &Value) -> Result<TaskFlow, Fault> {
        let Some(child) = node.children().first() else {
            let name = match node.kind() {
                NodeKind::Call(CallKind::Function(name)) => name.as_str(),
                _ => "function",
            };
            return Err(Fault::runtime(
                node.position(),
                format!("function `{name}` is not defined in use.functions"),
            ));
        };
        match self.exec_task(child, input.clone())? {
            TaskFlow::Blocked(block) => Ok(TaskFlow::Blocked(block)),
            TaskFlow::Done { output, directive } => {
                let directive = match directive {
                    Directive::End => Directive::End,
                    Directive::Continue | Directive::Exit => directive_of(node.then()),
                    Directive::Jump(name) => {
                        return Err(Fault::runtime(
                            node.position(),
                            format!("no sibling task `{name}`"),
                        ));
                    }
                };
                self.done(node, output, input, directive)
            }
        }
    }

    fn exec_effectful(&mut self, node: &Node, input: &Value) -> Result<TaskFlow, Fault> {
        let key = self.task_key(node);
        if let Some(result) = self.log.effect(&key) {
            return match result {
                crate::TaskResult::Output(output) => {
                    self.done(node, output.clone(), input, directive_of(node.then()))
                }
                crate::TaskResult::Fault(fault) => Err(fault.clone()),
            };
        }
        let pause = if matches!(node.kind(), NodeKind::Run(RunKind::Workflow)) {
            Pause::Child
        } else {
            Pause::Activity
        };
        self.pause(node, input.clone(), pause)
    }

    fn exec_pause(&mut self, node: &Node, input: &Value) -> Result<TaskFlow, Fault> {
        let key = self.task_key(node);
        if let Some(output) = self.log.released(&key) {
            return self.done(node, output.clone(), input, directive_of(node.then()));
        }
        let pause = match node.kind() {
            NodeKind::Wait => Pause::Timer,
            NodeKind::Listen => Pause::Events,
            NodeKind::Fork { compete } => Pause::Join { compete: *compete },
            _ => Pause::Activity,
        };
        self.pause(node, input.clone(), pause)
    }

    fn done(
        &mut self,
        node: &Node,
        raw_output: Value,
        input: &Value,
        directive: Directive,
    ) -> Result<TaskFlow, Fault> {
        let output = if let Some(source) =
            optional_string(node.body(), &["output", "as"], node.position())?
        {
            let vars = self.scope(Some(node), input, None);
            evaluate(source, &raw_output, &vars)
                .map_err(|message| Fault::expression(node.position(), message))?
        } else {
            raw_output
        };
        if let Some(source) = optional_string(node.body(), &["export", "as"], node.position())? {
            let vars = self.scope(Some(node), input, Some(&output));
            let exported = evaluate(source, &output, &vars)
                .map_err(|message| Fault::expression(node.position(), message))?;
            let Value::Object(exported) = exported else {
                return Err(Fault::expression(
                    node.position(),
                    "export.as must produce an object",
                ));
            };
            for (key, value) in exported {
                self.context.insert(key, value);
            }
        }
        Ok(TaskFlow::Done { output, directive })
    }

    fn raise_fault(&self, node: &Node) -> Result<Fault, Fault> {
        let Some(raise) = node.body().get("raise") else {
            return Err(Fault::runtime(
                node.position(),
                "raise task is missing `raise`",
            ));
        };
        let Some(error) = raise.get("error") else {
            return Err(Fault::runtime(node.position(), "raise is missing `error`"));
        };
        if let Some(name) = error.as_str() {
            let Some(resolved) = self
                .document
                .get("use")
                .and_then(|value| value.get("errors"))
                .and_then(|value| value.as_object())
                .and_then(|errors| errors.get(name))
            else {
                return Err(Fault::runtime(
                    node.position(),
                    format!("unknown error `{name}`"),
                ));
            };
            return Fault::from_error(node.position(), resolved);
        }
        Fault::from_error(node.position(), error)
    }

    fn retry_limit(&self, node: &Node) -> Result<u64, Fault> {
        let Some(policy) = self.retry_policy(node)? else {
            return Ok(0);
        };
        match policy.pointer("/limit/attempt/count") {
            None => Ok(1),
            Some(Value::Number(number)) => number.as_u64().ok_or_else(|| {
                Fault::runtime(
                    node.position(),
                    "retry limit.attempt.count must be a non-negative integer",
                )
            }),
            Some(_) => Err(Fault::runtime(
                node.position(),
                "retry limit.attempt.count must be a number",
            )),
        }
    }

    fn retry_policy(&self, node: &Node) -> Result<Option<Value>, Fault> {
        if !matches!(node.kind(), NodeKind::Try { retry: true }) {
            return Ok(None);
        }
        let Some(retry) = node
            .body()
            .get("catch")
            .and_then(|catch| catch.get("retry"))
        else {
            return Ok(None);
        };
        match retry {
            Value::Object(_) => Ok(Some(retry.clone())),
            Value::String(name) => {
                let policy = self
                    .document
                    .get("use")
                    .and_then(|value| value.get("retries"))
                    .and_then(|value| value.as_object())
                    .and_then(|retries| retries.get(name));
                let Some(policy) = policy else {
                    return Err(Fault::runtime(
                        node.position(),
                        format!("unknown retry `{name}`"),
                    ));
                };
                if !policy.is_object() {
                    return Err(Fault::runtime(
                        node.position(),
                        format!("retry `{name}` must be an object"),
                    ));
                }
                Ok(Some(policy.clone()))
            }
            _ => Err(Fault::runtime(
                node.position(),
                "retry must be a policy or a name",
            )),
        }
    }

    fn catch_matches(&self, node: &Node, fault: &Fault, input: &Value) -> Result<bool, Fault> {
        let Some(catch) = node.body().get("catch") else {
            return Ok(false);
        };
        if let Some(with) = catch.get("errors").and_then(|errors| errors.get("with")) {
            if let Some(expected) = with.get("type") {
                let Value::String(expected) = expected else {
                    return Err(Fault::runtime(
                        node.position(),
                        "errors.with.type must be a string",
                    ));
                };
                if expected != fault.error_type() {
                    return Ok(false);
                }
            }
            if let Some(expected) = with.get("status") {
                let Some(expected) = expected.as_u64() else {
                    return Err(Fault::runtime(
                        node.position(),
                        "errors.with.status must be a number",
                    ));
                };
                if expected != u64::from(fault.status()) {
                    return Ok(false);
                }
            }
        }
        let vars = self.scope(Some(node), input, None);
        if self.optional_bool(catch, "when", node, input, &vars)? == Some(false) {
            return Ok(false);
        }
        if self.optional_bool(catch, "exceptWhen", node, input, &vars)? == Some(true) {
            return Ok(false);
        }
        Ok(true)
    }

    fn retry_wanted(
        &self,
        node: &Node,
        input: &Value,
        attempt: u64,
        max_retries: u64,
    ) -> Result<bool, Fault> {
        if attempt >= max_retries {
            return Ok(false);
        }
        let Some(policy) = self.retry_policy(node)? else {
            return Ok(false);
        };
        let vars = self.scope(Some(node), input, None);
        if self.optional_bool(&policy, "when", node, input, &vars)? == Some(false) {
            return Ok(false);
        }
        if self.optional_bool(&policy, "exceptWhen", node, input, &vars)? == Some(true) {
            return Ok(false);
        }
        Ok(true)
    }

    fn optional_bool(
        &self,
        value: &Value,
        field: &str,
        node: &Node,
        input: &Value,
        vars: &BTreeMap<String, Value>,
    ) -> Result<Option<bool>, Fault> {
        let Some(source) = optional_string(value, &[field], node.position())? else {
            return Ok(None);
        };
        self.eval_bool(source, input, vars, node.position())
            .map(Some)
    }

    fn block(&self, node: &Node, input: Value, pause: Pause) -> Result<Block, Fault> {
        let timeout = self.task_timeout(node, &input)?;
        Ok(Block {
            key: self.task_key(node),
            name: node.name().map(str::to_string),
            pause,
            input,
            context: Value::Object(self.context.clone()),
            effectful: node.effectful(),
            kind: node.kind().clone(),
            timeout,
        })
    }

    fn task_key(&self, node: &Node) -> TaskKey {
        TaskKey::new(node.position(), ExecutionKey::new(self.frames.clone()))
    }

    fn key_for_attempt(&self, node: &Node, attempt: u64) -> TaskKey {
        let mut frames = self.frames.clone();
        frames.push(Frame::Attempt(attempt));
        TaskKey::new(node.position(), ExecutionKey::new(frames))
    }

    fn retry_block(&mut self, node: &Node, input: &Value, attempt: u64) -> Result<TaskFlow, Fault> {
        let delay = self.retry_wait(node, input, attempt)?;
        self.frames.push(Frame::Attempt(attempt));
        let paused = self.pause(node, input.clone(), Pause::Retry { attempt, delay });
        self.frames.pop();
        paused
    }

    fn retry_wait(&self, node: &Node, input: &Value, attempt: u64) -> Result<Duration, Fault> {
        let Some(policy) = self.retry_policy(node)? else {
            return Ok(Duration::ZERO);
        };
        let spec =
            retry_spec(&policy).map_err(|message| Fault::runtime(node.position(), message))?;
        let delay_ms = match spec.delay {
            Some(value) => self.duration_of(value, node.position(), Some(node), input)?,
            None => 0,
        };
        let increment_ms = match spec.increment {
            Some(value) => Some(self.duration_of(value, node.position(), Some(node), input)?),
            None => None,
        };
        let jitter_ms = match (spec.jitter_from, spec.jitter_to) {
            (Some(from), Some(to)) => Some((
                self.duration_of(from, node.position(), Some(node), input)?,
                self.duration_of(to, node.position(), Some(node), input)?,
            )),
            _ => None,
        };
        let millis = wait_ms(
            delay_ms,
            spec.kind,
            increment_ms,
            jitter_ms,
            attempt,
            self.jitter,
        )
        .map_err(|message| Fault::runtime(node.position(), message))?;
        Ok(Duration::from_millis(millis))
    }

    fn duration_of(
        &self,
        value: &Value,
        position: &str,
        node: Option<&Node>,
        input: &Value,
    ) -> Result<u64, Fault> {
        let literal = if let Value::String(text) = value {
            if let Some(source) = runtime_expression(text) {
                let vars = self.scope(node, input, None);
                match evaluate(source, input, &vars)
                    .map_err(|message| Fault::expression(position, message))?
                {
                    Value::String(iso) => Value::String(iso),
                    _ => {
                        return Err(Fault::expression(
                            position,
                            "duration expression must be an ISO 8601 string",
                        ));
                    }
                }
            } else {
                value.clone()
            }
        } else {
            value.clone()
        };
        duration_millis(&literal).map_err(|message| Fault::runtime(position, message))
    }

    fn workflow_timeout(&self, input: &Value) -> Result<Option<Fault>, Fault> {
        let Some(after) = self.timeout_after(self.document.get("timeout"), "/", None, input)?
        else {
            return Ok(None);
        };
        let Some(now) = self.now else {
            return Ok(None);
        };
        let Some(started) = self.log.workflow_started() else {
            return Ok(None);
        };
        if now >= started.saturating_add(after) {
            Ok(Some(Fault::timeout(
                "/",
                format!("timed out after {}ms", after.as_millis()),
            )))
        } else {
            Ok(None)
        }
    }

    fn task_timeout(&self, node: &Node, input: &Value) -> Result<Option<Duration>, Fault> {
        self.timeout_after(
            node.body().get("timeout"),
            node.position(),
            Some(node),
            input,
        )
    }

    fn timeout_after(
        &self,
        timeout: Option<&Value>,
        position: &str,
        node: Option<&Node>,
        input: &Value,
    ) -> Result<Option<Duration>, Fault> {
        let Some(timeout) = timeout else {
            return Ok(None);
        };
        let policy = match timeout {
            Value::String(name) => {
                let found = self
                    .document
                    .get("use")
                    .and_then(|value| value.get("timeouts"))
                    .and_then(|value| value.as_object())
                    .and_then(|timeouts| timeouts.get(name));
                let Some(found) = found else {
                    return Err(Fault::runtime(
                        position,
                        format!("unknown timeout `{name}`"),
                    ));
                };
                if !found.is_object() {
                    return Err(Fault::runtime(
                        position,
                        format!("timeout `{name}` must be an object"),
                    ));
                }
                found
            }
            Value::Object(_) => timeout,
            _ => {
                return Err(Fault::runtime(
                    position,
                    "timeout must be a policy or a name",
                ));
            }
        };
        if let Some(key) = policy.as_object().and_then(|object| {
            object
                .keys()
                .find(|key| key.as_str() != "after")
                .map(String::as_str)
        }) {
            return Err(Fault::runtime(
                position,
                format!("unknown timeout field `{key}`"),
            ));
        }
        let Some(after) = policy.get("after") else {
            return Err(Fault::runtime(position, "timeout.after is required"));
        };
        let millis = self.duration_of(after, position, node, input)?;
        Ok(Some(Duration::from_millis(millis)))
    }

    fn pause(&self, node: &Node, input: Value, pause: Pause) -> Result<TaskFlow, Fault> {
        let block = self.block(node, input, pause)?;
        if let Some(fault) = self.timeout_fault(&block) {
            return Err(fault);
        }
        Ok(TaskFlow::Blocked(block))
    }

    fn timeout_fault(&self, block: &Block) -> Option<Fault> {
        let after = block.timeout()?;
        let now = self.now?;
        let started = self.log.task_started(block.key())?;
        if now >= started.saturating_add(after) {
            Some(Fault::timeout(
                block.position(),
                format!("timed out after {}ms", after.as_millis()),
            ))
        } else {
            None
        }
    }

    fn scope(
        &self,
        node: Option<&Node>,
        input: &Value,
        output: Option<&Value>,
    ) -> BTreeMap<String, Value> {
        let mut map = BTreeMap::new();
        map.insert("context".to_string(), Value::Object(self.context.clone()));
        map.insert("input".to_string(), input.clone());
        map.insert(
            "workflow".to_string(),
            serde_json::json!({ "input": self.workflow_input }),
        );
        if let Some(node) = node {
            map.insert(
                "task".to_string(),
                serde_json::json!({
                    "name": node.name(),
                    "reference": node.position(),
                    "input": input,
                }),
            );
        }
        if let Some(output) = output {
            map.insert("output".to_string(), output.clone());
        }
        for binding in &self.bindings {
            for (key, value) in binding {
                map.insert(key.clone(), value.clone());
            }
        }
        map
    }

    fn eval_bool(
        &self,
        source: &str,
        dot: &Value,
        vars: &BTreeMap<String, Value>,
        position: &str,
    ) -> Result<bool, Fault> {
        match evaluate(source, dot, vars).map_err(|message| Fault::expression(position, message))? {
            Value::Bool(flag) => Ok(flag),
            _ => Err(Fault::expression(
                position,
                format!("`{source}` must be a boolean"),
            )),
        }
    }

    fn charge(&mut self, position: &str) -> Result<(), Fault> {
        self.steps += 1;
        if self.steps > MAX_INLINE_STEPS {
            return Err(Fault::runtime(
                position,
                format!("inline walk exceeded {MAX_INLINE_STEPS} steps"),
            ));
        }
        Ok(())
    }
}

fn directive_of(flow: &FlowDirective) -> Directive {
    match flow {
        FlowDirective::Continue => Directive::Continue,
        FlowDirective::Exit => Directive::Exit,
        FlowDirective::End => Directive::End,
        FlowDirective::Task(name) => Directive::Jump(name.clone()),
    }
}

fn catch_directive(node: &Node) -> Directive {
    match node.body().get("catch").and_then(|catch| catch.get("then")) {
        Some(Value::String(text)) => directive_of(&FlowDirective::parse(text)),
        _ => directive_of(node.then()),
    }
}

fn merge_object(input: &Value, assigned: Map<String, Value>) -> Value {
    let mut base = match input {
        Value::Object(object) => object.clone(),
        _ => Map::new(),
    };
    for (key, value) in assigned {
        base.insert(key, value);
    }
    Value::Object(base)
}

fn optional_string<'a>(
    value: &'a Value,
    path: &[&str],
    position: &str,
) -> Result<Option<&'a str>, Fault> {
    let mut current = value;
    for (depth, key) in path.iter().enumerate() {
        match current.get(*key) {
            None => return Ok(None),
            Some(next) if depth + 1 == path.len() => {
                return match next {
                    Value::String(text) => Ok(Some(text.as_str())),
                    _ => Err(Fault::expression(
                        position,
                        format!("`{}` must be a string", path.join(".")),
                    )),
                };
            }
            Some(next) => current = next,
        }
    }
    Ok(None)
}

fn var_name(node: &Node, path: &[&str], default: &str) -> Result<String, Fault> {
    match optional_string(node.body(), path, node.position())? {
        None => Ok(default.to_string()),
        Some(name) if is_ident(name) => Ok(name.to_string()),
        Some(_) => Err(Fault::runtime(
            node.position(),
            format!("`{}` must be an identifier", path.join(".")),
        )),
    }
}

fn error_binding(node: &Node) -> Result<String, Fault> {
    var_name(node, &["catch", "as"], "error")
}

fn is_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch.is_ascii_alphabetic() || ch == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}
