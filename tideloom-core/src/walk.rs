//! Run an immutable definition from the root, or resume it from a continuation.
//!
//! The interpreter keeps an explicit stack of frames, one per enclosing
//! control-flow task. Control-flow tasks run inline. When the run reaches a
//! block boundary it stops, and the frames plus the pending block become a
//! [`Continuation`]. [`resume`] takes that value and what happened to the
//! block, and continues from exactly there. Nothing is re-walked from the
//! root, and inline tasks do not run twice.
//!
//! Expressions evaluated here are a closed subset: `.path` and `$name`,
//! literals, comparisons, `+ - * /`, `and` / `or` / `not`, parentheses, arrays,
//! objects, and `|`. A `set` string is literal text unless it contains
//! `${ ... }`. Full jq is not implemented; an unsupported expression is an
//! expression fault.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

use crate::CallKind;
use crate::Continuation;
use crate::Definition;
use crate::Fault;
use crate::FlowDirective;
use crate::JitterSample;
use crate::Node;
use crate::NodeKind;
use crate::Resume;
use crate::RunKind;
use crate::TaskKey;
use crate::Timestamp;
use crate::continuation::Body;
use crate::continuation::ForFrame;
use crate::continuation::Frame;
use crate::continuation::FunctionFrame;
use crate::continuation::Pending;
use crate::continuation::Root;
use crate::continuation::SCHEMA;
use crate::continuation::SeqFrame;
use crate::continuation::TryFrame;
use crate::continuation::TryPhase;
use crate::expr::evaluate;
use crate::expr::evaluate_data;
use crate::expr::runtime_expression;
use crate::key::ExecutionKey;
use crate::retry::duration_millis;
use crate::retry::retry_spec;
use crate::retry::wait_ms;
use crate::spec::BlockDetail;
use crate::spec::Branch;
use crate::spec::ChildRef;
use crate::spec::EventFilter;
use crate::spec::ListenSpec;
use crate::spec::ListenStrategy;

/// Stop an inline loop that never reaches a block boundary.
const MAX_INLINE_STEPS: u32 = 10_000;

/// Why a [`Block`] was handed to a runner.
///
/// These are the ADR-0001 pause reasons. [`Pause::Activity`] and
/// [`Pause::Child`] are effectful and record a `task_execution`. The others
/// are blocking only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
/// The run executed every non-boundary task before this node. `input` is the
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
    detail: BlockDetail,
}

impl Block {
    /// JSON Pointer plus execution key. This is the `task_execution` identity.
    pub fn key(&self) -> &TaskKey {
        &self.key
    }

    /// JSON Pointer of the boundary node.
    pub fn position(&self) -> &str {
        self.key.position()
    }

    /// Ancestor `for` indexes, `try` attempts, and jump passes, outermost first.
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
    /// `None` when the task has no timeout. The runner keeps `started_at` on
    /// the continuation with [`Continuation::start_task`]. The run does not
    /// sleep.
    #[must_use]
    pub fn timeout_at(&self, started_at: Timestamp) -> Option<Timestamp> {
        self.timeout.map(|after| started_at.saturating_add(after))
    }

    /// Pause-specific data: the `wait` duration, `fork` branches, the child
    /// workflow, or the `listen` subscription.
    #[must_use]
    pub fn detail(&self) -> &BlockDetail {
        &self.detail
    }
}

impl Pause {
    /// Instant a retry pause may be released: `started_at + delay`.
    ///
    /// `started_at` is when the runner first observed this pause. The delay is
    /// computed once, when the pause is created, and stored on the
    /// continuation; a later [`Resume::Tick`] returns the same delay. Other
    /// pause reasons return `None`.
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

    fn effectful(self) -> bool {
        matches!(self, Self::Activity | Self::Child)
    }
}

/// Knobs that are not the definition or the continuation.
///
/// [`start`] and [`resume`] use [`WalkOptions::default`], which pins jitter
/// to `jitter.from` and supplies no clock.
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
    /// [`JitterSample::FROM`] is `from`. [`JitterSample::TO`] is `to`. The
    /// sample is read when a retry pause is created; the resulting delay is
    /// stored on the continuation.
    #[must_use]
    pub fn with_jitter(mut self, sample: JitterSample) -> Self {
        self.jitter = sample;
        self
    }

    /// Clock reading for workflow and task deadlines.
    ///
    /// [`start_with`] records it as the workflow start. A new block records it
    /// as the task start. [`Resume::Tick`] compares it with those starts.
    /// Without it, a configured timeout is reported on the block and is not
    /// enforced.
    #[must_use]
    pub fn with_now(mut self, now: Timestamp) -> Self {
        self.now = Some(now);
        self
    }
}

/// What one run decided.
///
/// `Blocked` carries the whole continuation by value. One outcome is produced
/// per block, so the size difference between variants does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The workflow finished. `output` has workflow `output.as` applied.
    Completed {
        /// Workflow output.
        output: Value,
        /// `$context` after every export.
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
        /// Everything needed to continue after that block.
        continuation: Continuation,
    },
}

/// Run `definition` from the root with the workflow `input`.
///
/// Inline tasks run until the first block boundary. An inline cycle that
/// never hits a boundary faults after 10_000 task entries.
///
/// # Examples
///
/// ```
/// use tideloom_core::Definition;
/// use tideloom_core::Outcome;
/// use tideloom_core::Pause;
/// use tideloom_core::start;
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
/// let outcome = start(&definition, &serde_json::json!({}));
/// let Outcome::Blocked {
///     block,
///     continuation,
/// } = outcome
/// else {
///     panic!("expected a block");
/// };
/// assert_eq!(block.position(), "/do/1/notify");
/// assert!(block.effectful());
/// assert!(matches!(block.pause(), Pause::Activity));
/// assert_eq!(block.input()["color"], "red");
/// assert_eq!(continuation.position(), "/do/1/notify");
/// ```
#[must_use]
pub fn start(definition: &Definition, input: &Value) -> Outcome {
    start_with(definition, input, WalkOptions::default())
}

/// [`start`] with an explicit jitter sample and clock reading.
///
/// `now`, when given, is recorded as the workflow start.
#[must_use]
pub fn start_with(definition: &Definition, input: &Value, options: WalkOptions) -> Outcome {
    let mut interpreter = Interpreter::new(
        definition,
        State {
            root: Root::Workflow,
            workflow_input: input.clone(),
            context: Map::new(),
            frames: Vec::new(),
            workflow_started: options.now,
            task_started: None,
        },
        options,
    );
    let first = interpreter.workflow_timeout().and_then(|()| {
        let started = interpreter.workflow_input(input)?;
        interpreter.start_seq(definition.root(), Body::Root, Value::Null, started)
    });
    interpreter.run(first)
}

/// Continue a run from `continuation` after `event` happened to its block.
///
/// Inline tasks after the block run until the next boundary. A
/// [`Resume::Tick`] with no reached deadline returns the same block and the
/// same continuation.
///
/// # Examples
///
/// ```
/// use tideloom_core::Definition;
/// use tideloom_core::Outcome;
/// use tideloom_core::Resume;
/// use tideloom_core::resume;
/// use tideloom_core::start;
///
/// let yaml = r#"
/// document:
///   dsl: '1.0.0'
///   namespace: default
///   name: paint
///   version: '0.1.0'
/// do:
///   - notify:
///       emit: {}
///   - seal:
///       set:
///         sealed: true
/// "#;
/// let definition = Definition::from_yaml(yaml).unwrap();
/// let Outcome::Blocked { continuation, .. } = start(&definition, &serde_json::json!({})) else {
///     panic!("expected a block");
/// };
/// let outcome = resume(
///     &definition,
///     continuation,
///     Resume::Output(serde_json::json!({"sent": true})),
/// );
/// let Outcome::Completed { output, .. } = outcome else {
///     panic!("expected completion");
/// };
/// assert_eq!(output, serde_json::json!({"sealed": true}));
/// ```
#[must_use]
pub fn resume(definition: &Definition, continuation: Continuation, event: Resume) -> Outcome {
    resume_with(definition, continuation, event, WalkOptions::default())
}

/// [`resume`] with an explicit jitter sample and clock reading.
///
/// The workflow `timeout` is checked first on every resume. Task, attempt,
/// and retry-window deadlines are checked on [`Resume::Tick`]. An
/// [`Resume::Output`] or [`Resume::Release`] is applied even when a deadline
/// has passed: a finished task is kept.
#[must_use]
pub fn resume_with(
    definition: &Definition,
    continuation: Continuation,
    event: Resume,
    options: WalkOptions,
) -> Outcome {
    let Continuation {
        schema,
        root,
        workflow_input,
        context,
        frames,
        pending,
        workflow_started,
        task_started,
    } = continuation;
    if schema != SCHEMA {
        return Outcome::Faulted {
            fault: Fault::configuration(
                &pending.position,
                format!("continuation schema {schema} is not supported, expected {SCHEMA}"),
            ),
            context: Value::Object(context),
        };
    }
    let mut interpreter = Interpreter::new(
        definition,
        State {
            root,
            workflow_input,
            context,
            frames,
            workflow_started,
            task_started,
        },
        options,
    );
    let first = interpreter
        .workflow_timeout()
        .and_then(|()| interpreter.apply(pending, event));
    interpreter.run(first)
}

/// Run one task of `definition` as its own root, for example a `fork` branch.
///
/// The task's transformed output is the run output. Workflow `input.from`,
/// `output.as`, and `timeout` do not apply. A `then` that names a sibling
/// faults, since the task has none here.
#[must_use]
pub fn start_at(definition: &Definition, position: &str, input: &Value) -> Outcome {
    start_at_with(definition, position, input, WalkOptions::default())
}

/// [`start_at`] with an explicit jitter sample and clock reading.
#[must_use]
pub fn start_at_with(
    definition: &Definition,
    position: &str,
    input: &Value,
    options: WalkOptions,
) -> Outcome {
    let interpreter = Interpreter::new(
        definition,
        State {
            root: Root::Task {
                position: position.to_string(),
            },
            workflow_input: input.clone(),
            context: Map::new(),
            frames: Vec::new(),
            workflow_started: options.now,
            task_started: None,
        },
        options,
    );
    let first = interpreter.node(position).map(|node| Action::Enter {
        node,
        input: input.clone(),
    });
    interpreter.run(first)
}

/// What the interpreter does next.
///
/// `Stop` carries a whole `Block`; one is built per stop, so the variant size
/// difference is accepted.
#[allow(clippy::large_enum_variant)]
enum Action<'a> {
    /// Run `node` with its raw input.
    Enter { node: &'a Node, input: Value },
    /// Hand a result to the innermost frame.
    Up(Up),
    /// The run is over for now.
    Stop(Stop),
}

/// A result travelling to the innermost frame.
enum Up {
    /// A task directly below the frame finished.
    Task { output: Value, directive: Directive },
    /// A task list directly below the frame finished.
    Seq(Flow),
    /// A fault nobody below the frame caught.
    Fault(Fault),
    /// The retry backoff of the frame is over.
    Released,
}

#[allow(clippy::large_enum_variant)]
enum Stop {
    Completed { output: Value },
    Faulted { fault: Fault },
    Blocked { block: Block, pending: Pending },
}

enum Directive {
    Continue,
    Exit,
    End,
    Jump(String),
}

/// How a task list finished.
enum Flow {
    Finished(Value),
    Exited(Value),
    Ended(Value),
}

/// What a `try` decided about a fault in its body.
enum Decision {
    Propagate,
    Backoff(Duration),
    Catch,
}

/// The parts of a run that survive a stop.
struct State {
    root: Root,
    workflow_input: Value,
    context: Map<String, Value>,
    frames: Vec<Frame>,
    workflow_started: Option<Timestamp>,
    task_started: Option<Timestamp>,
}

struct Interpreter<'a> {
    nodes: HashMap<&'a str, &'a Node>,
    document: &'a Value,
    state: State,
    steps: u32,
    jitter: JitterSample,
    now: Option<Timestamp>,
}

impl<'a> Interpreter<'a> {
    fn new(definition: &'a Definition, state: State, options: WalkOptions) -> Self {
        Self {
            nodes: definition
                .nodes()
                .map(|node| (node.position(), node))
                .collect(),
            document: definition.root().body(),
            state,
            steps: 0,
            jitter: options.jitter,
            now: options.now,
        }
    }

    // ========================================
    // Driving loop
    // ========================================

    fn run(mut self, first: Result<Action<'a>, Fault>) -> Outcome {
        let mut action = match first {
            Ok(action) => action,
            Err(fault) => Action::Up(Up::Fault(fault)),
        };
        loop {
            let next = match action {
                Action::Stop(stop) => return self.finish(stop),
                Action::Enter { node, input } => self.enter(node, input),
                Action::Up(up) => self.deliver(up),
            };
            action = match next {
                Ok(action) => action,
                Err(fault) => Action::Up(Up::Fault(fault)),
            };
        }
    }

    fn finish(self, stop: Stop) -> Outcome {
        let context = Value::Object(self.state.context.clone());
        match stop {
            Stop::Completed { output } => Outcome::Completed { output, context },
            Stop::Faulted { fault } => Outcome::Faulted { fault, context },
            Stop::Blocked { block, pending } => Outcome::Blocked {
                block,
                continuation: Continuation {
                    schema: SCHEMA,
                    root: self.state.root,
                    workflow_input: self.state.workflow_input,
                    context: self.state.context,
                    frames: self.state.frames,
                    pending,
                    workflow_started: self.state.workflow_started,
                    task_started: self.state.task_started,
                },
            },
        }
    }

    /// Hand `up` to the innermost frame, or finish the workflow when there is none.
    fn deliver(&mut self, up: Up) -> Result<Action<'a>, Fault> {
        let Some(frame) = self.state.frames.pop() else {
            return self.deliver_to_root(up);
        };
        match frame {
            Frame::Seq(seq) => self.on_seq(seq, up),
            Frame::For(frame) => self.on_for(frame, up),
            Frame::Try(frame) => self.on_try(frame, up),
            Frame::Function(frame) => self.on_function(frame, up),
        }
    }

    fn deliver_to_root(&mut self, up: Up) -> Result<Action<'a>, Fault> {
        if let Root::Task { position } = &self.state.root {
            return match up {
                Up::Task { output, directive } => match directive {
                    Directive::Jump(name) => Err(Fault::runtime(
                        position,
                        format!("no sibling task `{name}`"),
                    )),
                    Directive::Continue | Directive::Exit | Directive::End => {
                        Ok(Action::Stop(Stop::Completed { output }))
                    }
                },
                Up::Fault(fault) => Ok(Action::Stop(Stop::Faulted { fault })),
                Up::Seq(_) | Up::Released => Err(internal(position, "no frame to receive a list")),
            };
        }
        match up {
            Up::Seq(flow) => {
                let raw = match flow {
                    Flow::Finished(value) | Flow::Exited(value) | Flow::Ended(value) => value,
                };
                let output = self.workflow_output(raw)?;
                Ok(Action::Stop(Stop::Completed { output }))
            }
            Up::Fault(fault) => Ok(Action::Stop(Stop::Faulted { fault })),
            Up::Task { .. } | Up::Released => Err(internal("/", "no frame to receive a result")),
        }
    }

    // ========================================
    // Resuming a pending block
    // ========================================

    fn apply(&mut self, pending: Pending, event: Resume) -> Result<Action<'a>, Fault> {
        let node = self.node(&pending.position)?;
        let effectful = pending.pause.effectful();
        match event {
            Resume::Output(output) if effectful => self.complete(node, &pending, output),
            Resume::Fault(fault) if effectful => Ok(Action::Up(Up::Fault(fault))),
            Resume::Release(_) if matches!(pending.pause, Pause::Retry { .. }) => {
                Ok(Action::Up(Up::Released))
            }
            Resume::Release(output) if !effectful => self.complete(node, &pending, output),
            Resume::Tick => self.tick(node, pending),
            Resume::Output(_) | Resume::Fault(_) => Ok(Action::Stop(Stop::Faulted {
                fault: Fault::runtime(
                    node.position(),
                    "a blocking pause is resumed with Release, not Output or Fault",
                ),
            })),
            Resume::Release(_) => Ok(Action::Stop(Stop::Faulted {
                fault: Fault::runtime(
                    node.position(),
                    "an effectful task is resumed with Output or Fault, not Release",
                ),
            })),
        }
    }

    /// Finish the pending task with `raw_output` and hand it to its frame.
    fn complete(
        &mut self,
        node: &'a Node,
        pending: &Pending,
        raw_output: Value,
    ) -> Result<Action<'a>, Fault> {
        let (output, directive) =
            self.done(node, raw_output, &pending.input, directive_of(node.then()))?;
        Ok(Action::Up(Up::Task { output, directive }))
    }

    /// Re-check every deadline that applies to the pending block.
    fn tick(&mut self, node: &'a Node, pending: Pending) -> Result<Action<'a>, Fault> {
        if let Some(after) = self.task_timeout(node, &pending.input)?
            && self.deadline_reached(self.state.task_started, after)
        {
            return Ok(Action::Up(Up::Fault(Fault::timeout(
                node.position(),
                format!("timed out after {}ms", after.as_millis()),
            ))));
        }
        for frame in self.state.frames.iter().rev() {
            let Frame::Try(frame) = frame else {
                continue;
            };
            if frame.phase != TryPhase::Body {
                continue;
            }
            let try_node = self.node(&frame.position)?;
            if let Some(after) =
                self.retry_limit_duration(try_node, &frame.input, "/limit/attempt/duration")?
                && self.deadline_reached(self.state.task_started, after)
            {
                return Ok(Action::Up(Up::Fault(Fault::timeout(
                    node.position(),
                    format!("attempt timed out after {}ms", after.as_millis()),
                ))));
            }
        }
        if matches!(pending.pause, Pause::Retry { .. })
            && !self.retry_window_open(node, &pending.input)?
        {
            let Some(Frame::Try(frame)) = self.state.frames.pop() else {
                return Err(internal(node.position(), "retry pause without a try frame"));
            };
            return self.try_declined(node, frame);
        }
        let block = self.block(node, &pending)?;
        Ok(Action::Stop(Stop::Blocked { block, pending }))
    }

    // ========================================
    // Entering a task
    // ========================================

    fn enter(&mut self, node: &'a Node, raw_input: Value) -> Result<Action<'a>, Fault> {
        self.charge(node.position())?;
        if let Some(source) = optional_string(node.body(), &["if"], node.position())? {
            let vars = self.scope(Some(node), &raw_input, None);
            if !self.eval_bool(source, &raw_input, &vars, node.position())? {
                return Ok(Action::Up(Up::Task {
                    output: raw_input,
                    directive: directive_of(node.then()),
                }));
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

    fn dispatch(&mut self, node: &'a Node, input: Value) -> Result<Action<'a>, Fault> {
        match node.kind() {
            NodeKind::Set => self.exec_set(node, &input),
            NodeKind::Raise => Err(self.raise_fault(node)?),
            NodeKind::Switch => self.exec_switch(node, &input),
            NodeKind::Do => self.start_seq(node, Body::Do, input.clone(), input),
            NodeKind::For => self.enter_for(node, input),
            NodeKind::Try { .. } => self.enter_try(node, input),
            NodeKind::Wait => self.block_at(node, input, Pause::Timer),
            NodeKind::Listen => self.block_at(node, input, Pause::Events),
            NodeKind::Fork { compete } => {
                self.block_at(node, input, Pause::Join { compete: *compete })
            }
            NodeKind::Call(CallKind::Function(_)) => self.enter_function(node, input),
            NodeKind::Run(RunKind::Workflow) => self.block_at(node, input, Pause::Child),
            NodeKind::Call(_) | NodeKind::Emit | NodeKind::Run(_) => {
                self.block_at(node, input, Pause::Activity)
            }
            NodeKind::Root | NodeKind::SwitchCase { .. } => Err(Fault::runtime(
                node.position(),
                "internal node is not a task",
            )),
        }
    }

    fn exec_set(&mut self, node: &'a Node, input: &Value) -> Result<Action<'a>, Fault> {
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
        // The task output is exactly the evaluated `set` object. Input keys
        // that were not assigned are not copied forward.
        let (output, directive) = self.done(
            node,
            Value::Object(assigned),
            input,
            directive_of(node.then()),
        )?;
        Ok(Action::Up(Up::Task { output, directive }))
    }

    fn exec_switch(&mut self, node: &'a Node, input: &Value) -> Result<Action<'a>, Fault> {
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
                let (output, directive) =
                    self.done(node, input.clone(), input, directive_of(case.then()))?;
                return Ok(Action::Up(Up::Task { output, directive }));
            }
        }
        // No case matched. The switch finishes with its input and follows its
        // own `then`, which is `continue` when the task omits one.
        let (output, directive) =
            self.done(node, input.clone(), input, directive_of(node.then()))?;
        Ok(Action::Up(Up::Task { output, directive }))
    }

    fn block_at(
        &mut self,
        node: &'a Node,
        input: Value,
        pause: Pause,
    ) -> Result<Action<'a>, Fault> {
        let pending = Pending {
            position: node.position().to_string(),
            input,
            pause,
        };
        let block = self.block(node, &pending)?;
        self.state.task_started = self.now;
        Ok(Action::Stop(Stop::Blocked { block, pending }))
    }

    fn block(&self, node: &Node, pending: &Pending) -> Result<Block, Fault> {
        let timeout = self.task_timeout(node, &pending.input)?;
        let detail = self.detail(node, pending)?;
        Ok(Block {
            key: TaskKey::new(node.position(), self.execution_key()),
            name: node.name().map(str::to_string),
            pause: pending.pause,
            input: pending.input.clone(),
            context: Value::Object(self.state.context.clone()),
            effectful: node.effectful(),
            kind: node.kind().clone(),
            timeout,
            detail,
        })
    }

    // ========================================
    // Block detail
    // ========================================

    fn detail(&self, node: &Node, pending: &Pending) -> Result<BlockDetail, Fault> {
        Ok(match pending.pause {
            Pause::Activity | Pause::Retry { .. } => BlockDetail::None,
            Pause::Timer => BlockDetail::Wait {
                after: self.wait_after(node, &pending.input)?,
            },
            Pause::Join { .. } => BlockDetail::Branches(
                node.children()
                    .iter()
                    .map(|child| Branch {
                        position: child.position().to_string(),
                        name: child.name().unwrap_or_default().to_string(),
                    })
                    .collect(),
            ),
            Pause::Child => BlockDetail::Child(self.child_ref(node, &pending.input)?),
            Pause::Events => BlockDetail::Listen(listen_spec(node)?),
        })
    }

    /// The `wait` duration. An empty `wait: {}` is zero.
    fn wait_after(&self, node: &Node, input: &Value) -> Result<Duration, Fault> {
        match node.body().get("wait") {
            None => Ok(Duration::ZERO),
            Some(Value::Object(object)) if object.is_empty() => Ok(Duration::ZERO),
            Some(value) => {
                let millis = self.duration_of(value, node.position(), Some(node), input)?;
                Ok(Duration::from_millis(millis))
            }
        }
    }

    fn child_ref(&self, node: &Node, input: &Value) -> Result<ChildRef, Fault> {
        let run = node
            .body()
            .get("run")
            .and_then(Value::as_object)
            .ok_or_else(|| Fault::runtime(node.position(), "run task is missing `run`"))?;
        let workflow = run
            .get("workflow")
            .and_then(Value::as_object)
            .ok_or_else(|| Fault::runtime(node.position(), "run.workflow must be an object"))?;
        let field = |name: &str| -> Result<String, Fault> {
            match workflow.get(name) {
                Some(Value::String(text)) => Ok(text.clone()),
                _ => Err(Fault::runtime(
                    node.position(),
                    format!("run.workflow.{name} must be a string"),
                )),
            }
        };
        let child_input = match workflow.get("input") {
            None => input.clone(),
            Some(template) => {
                let vars = self.scope(Some(node), input, None);
                evaluate_data(template, input, &vars)
                    .map_err(|message| Fault::expression(node.position(), message))?
            }
        };
        let await_result = match run.get("await") {
            None => true,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => {
                return Err(Fault::runtime(
                    node.position(),
                    "run.await must be a boolean",
                ));
            }
        };
        Ok(ChildRef {
            namespace: field("namespace")?,
            name: field("name")?,
            version: field("version")?,
            input: child_input,
            await_result,
        })
    }

    // ========================================
    // Task lists
    // ========================================

    /// Push a list frame owned by `owner` and enter its first child with `data`.
    fn start_seq(
        &mut self,
        owner: &'a Node,
        body: Body,
        input: Value,
        data: Value,
    ) -> Result<Action<'a>, Fault> {
        let seq = SeqFrame {
            owner: owner.position().to_string(),
            body,
            next: 0,
            pass: 0,
            input,
        };
        let children = self.seq_children(&seq)?;
        match children.first() {
            Some(child) => {
                self.state.frames.push(Frame::Seq(seq));
                Ok(Action::Enter {
                    node: child,
                    input: data,
                })
            }
            None => self.seq_finished(seq, Flow::Finished(data)),
        }
    }

    fn seq_children(&self, seq: &SeqFrame) -> Result<&'a [Node], Fault> {
        let owner = self.node(&seq.owner)?;
        Ok(match seq.body {
            Body::Root | Body::Do | Body::Iteration => owner.children(),
            Body::Attempt => try_bodies(owner).0,
            Body::Catch => try_bodies(owner).1,
        })
    }

    fn on_seq(&mut self, mut seq: SeqFrame, up: Up) -> Result<Action<'a>, Fault> {
        let (output, directive) = match up {
            Up::Task { output, directive } => (output, directive),
            Up::Fault(fault) => return Ok(Action::Up(Up::Fault(fault))),
            Up::Seq(_) | Up::Released => {
                return Err(internal(&seq.owner, "task list received a list result"));
            }
        };
        let children = self.seq_children(&seq)?;
        let flow = match directive {
            Directive::Continue => {
                seq.next += 1;
                if let Some(child) = children.get(seq.next) {
                    self.state.frames.push(Frame::Seq(seq));
                    return Ok(Action::Enter {
                        node: child,
                        input: output,
                    });
                }
                Flow::Finished(output)
            }
            Directive::Exit => Flow::Exited(output),
            Directive::End => Flow::Ended(output),
            Directive::Jump(name) => {
                let from = children
                    .get(seq.next)
                    .map_or(seq.owner.as_str(), Node::position);
                let index = children
                    .iter()
                    .position(|child| child.name() == Some(name.as_str()))
                    .ok_or_else(|| Fault::runtime(from, format!("no sibling task `{name}`")))?;
                seq.next = index;
                seq.pass += 1;
                self.state.frames.push(Frame::Seq(seq));
                return Ok(Action::Enter {
                    node: &children[index],
                    input: output,
                });
            }
        };
        self.seq_finished(seq, flow)
    }

    /// The list is over. A `do` owner finishes itself; other owners get the flow.
    fn seq_finished(&mut self, seq: SeqFrame, flow: Flow) -> Result<Action<'a>, Fault> {
        match seq.body {
            Body::Root | Body::Iteration | Body::Attempt | Body::Catch => {
                Ok(Action::Up(Up::Seq(flow)))
            }
            Body::Do => {
                let node = self.node(&seq.owner)?;
                let (raw, directive) = flow_directive(flow, directive_of(node.then()));
                let (output, directive) = self.done(node, raw, &seq.input, directive)?;
                Ok(Action::Up(Up::Task { output, directive }))
            }
        }
    }

    // ========================================
    // for
    // ========================================

    fn enter_for(&mut self, node: &'a Node, input: Value) -> Result<Action<'a>, Fault> {
        let source = optional_string(node.body(), &["for", "in"], node.position())?
            .ok_or_else(|| Fault::runtime(node.position(), "for.in is missing"))?;
        let vars = self.scope(Some(node), &input, None);
        let collection = evaluate(source, &input, &vars)
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
        let frame = ForFrame {
            position: node.position().to_string(),
            input: input.clone(),
            items,
            index: 0,
            each,
            at,
        };
        self.for_advance(node, frame, 0, input)
    }

    /// Start iteration `index` with `data`, or finish the loop.
    fn for_advance(
        &mut self,
        node: &'a Node,
        mut frame: ForFrame,
        index: u64,
        data: Value,
    ) -> Result<Action<'a>, Fault> {
        let exhausted = usize::try_from(index).is_ok_and(|index| index < frame.items.len());
        if !exhausted {
            return self.for_done(node, frame, data, directive_of(node.then()));
        }
        frame.index = index;
        let input = frame.input.clone();
        let while_expr = optional_string(node.body(), &["while"], node.position())?;
        // The loop variables must be bound while `while` is evaluated.
        self.state.frames.push(Frame::For(frame));
        let proceed = match while_expr {
            Some(source) => {
                let vars = self.scope(Some(node), &input, None);
                self.eval_bool(source, &data, &vars, node.position())
            }
            None => Ok(true),
        };
        let proceed = match proceed {
            Ok(proceed) => proceed,
            Err(fault) => {
                self.state.frames.pop();
                return Err(fault);
            }
        };
        if proceed {
            return self.start_seq(node, Body::Iteration, Value::Null, data);
        }
        let Some(Frame::For(frame)) = self.state.frames.pop() else {
            return Err(internal(node.position(), "for frame is missing"));
        };
        self.for_done(node, frame, data, directive_of(node.then()))
    }

    fn for_done(
        &mut self,
        node: &'a Node,
        frame: ForFrame,
        data: Value,
        directive: Directive,
    ) -> Result<Action<'a>, Fault> {
        let (output, directive) = self.done(node, data, &frame.input, directive)?;
        Ok(Action::Up(Up::Task { output, directive }))
    }

    fn on_for(&mut self, frame: ForFrame, up: Up) -> Result<Action<'a>, Fault> {
        let node = self.node(&frame.position)?;
        match up {
            Up::Seq(Flow::Finished(output)) => {
                let next = frame.index + 1;
                self.for_advance(node, frame, next, output)
            }
            Up::Seq(Flow::Exited(output)) => {
                self.for_done(node, frame, output, directive_of(node.then()))
            }
            Up::Seq(Flow::Ended(output)) => self.for_done(node, frame, output, Directive::End),
            Up::Fault(fault) => Ok(Action::Up(Up::Fault(fault))),
            Up::Task { .. } | Up::Released => {
                Err(internal(node.position(), "for received a task result"))
            }
        }
    }

    // ========================================
    // try
    // ========================================

    fn enter_try(&mut self, node: &'a Node, input: Value) -> Result<Action<'a>, Fault> {
        // Validate the policy before running the body, so a bad count faults here.
        self.retry_limit(node)?;
        let frame = TryFrame {
            position: node.position().to_string(),
            input: input.clone(),
            attempt: 0,
            phase: TryPhase::Body,
            fault: None,
            error_as: error_binding(node)?,
            retry_started: None,
        };
        self.state.frames.push(Frame::Try(frame));
        self.start_seq(node, Body::Attempt, Value::Null, input)
    }

    fn on_try(&mut self, mut frame: TryFrame, up: Up) -> Result<Action<'a>, Fault> {
        let node = self.node(&frame.position)?;
        match up {
            Up::Seq(flow) => {
                let default = match frame.phase {
                    TryPhase::Body => directive_of(node.then()),
                    TryPhase::Catch => catch_directive(node),
                    TryPhase::Backoff => {
                        return Err(internal(node.position(), "list result during backoff"));
                    }
                };
                let (raw, directive) = flow_directive(flow, default);
                let (output, directive) = self.done(node, raw, &frame.input, directive)?;
                Ok(Action::Up(Up::Task { output, directive }))
            }
            Up::Fault(fault) => match frame.phase {
                TryPhase::Body => self.try_failed(node, frame, fault),
                TryPhase::Backoff | TryPhase::Catch => Ok(Action::Up(Up::Fault(fault))),
            },
            Up::Released => {
                if frame.phase != TryPhase::Backoff {
                    return Err(internal(node.position(), "release without a backoff"));
                }
                frame.attempt += 1;
                frame.phase = TryPhase::Body;
                frame.fault = None;
                frame.retry_started = frame.retry_started.or(self.state.task_started);
                let input = frame.input.clone();
                self.state.frames.push(Frame::Try(frame));
                self.start_seq(node, Body::Attempt, Value::Null, input)
            }
            Up::Task { .. } => Err(internal(node.position(), "try received a task result")),
        }
    }

    /// The body faulted. Decide between retry, catch, and propagation.
    fn try_failed(
        &mut self,
        node: &'a Node,
        mut frame: TryFrame,
        fault: Fault,
    ) -> Result<Action<'a>, Fault> {
        frame.fault = Some(fault.clone());
        let input = frame.input.clone();
        let attempt = frame.attempt;
        // The frame goes back first so `$error` is bound for the policy expressions.
        let depth = self.state.frames.len();
        self.state.frames.push(Frame::Try(frame));
        let decision = match self.try_decide(node, &input, attempt, &fault) {
            Ok(decision) => decision,
            Err(error) => {
                self.state.frames.truncate(depth);
                return Err(error);
            }
        };
        let Some(Frame::Try(frame)) = self.state.frames.last_mut() else {
            return Err(internal(node.position(), "try frame is missing"));
        };
        match decision {
            Decision::Propagate => {
                self.state.frames.pop();
                Ok(Action::Up(Up::Fault(fault)))
            }
            Decision::Backoff(delay) => {
                frame.phase = TryPhase::Backoff;
                self.block_at(node, input, Pause::Retry { attempt, delay })
            }
            Decision::Catch => {
                frame.phase = TryPhase::Catch;
                self.start_seq(node, Body::Catch, Value::Null, input)
            }
        }
    }

    fn try_decide(
        &self,
        node: &Node,
        input: &Value,
        attempt: u64,
        fault: &Fault,
    ) -> Result<Decision, Fault> {
        if !self.catch_matches(node, fault, input)? {
            return Ok(Decision::Propagate);
        }
        let max_retries = self.retry_limit(node)?;
        let retry = self.retry_wanted(node, input, attempt, max_retries)?;
        if retry && self.retry_window_open(node, input)? {
            return Ok(Decision::Backoff(self.retry_wait(node, input, attempt)?));
        }
        if try_bodies(node).1.is_empty() {
            return Ok(Decision::Propagate);
        }
        Ok(Decision::Catch)
    }

    /// The retry window closed during a backoff. Catch the stored fault or propagate it.
    fn try_declined(&mut self, node: &'a Node, mut frame: TryFrame) -> Result<Action<'a>, Fault> {
        let Some(fault) = frame.fault.clone() else {
            return Err(internal(node.position(), "backoff without a fault"));
        };
        if try_bodies(node).1.is_empty() {
            return Ok(Action::Up(Up::Fault(fault)));
        }
        frame.phase = TryPhase::Catch;
        let input = frame.input.clone();
        self.state.frames.push(Frame::Try(frame));
        self.start_seq(node, Body::Catch, Value::Null, input)
    }

    // ========================================
    // call of a use.functions entry
    // ========================================

    fn enter_function(&mut self, node: &'a Node, input: Value) -> Result<Action<'a>, Fault> {
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
        self.state.frames.push(Frame::Function(FunctionFrame {
            position: node.position().to_string(),
            input: input.clone(),
        }));
        Ok(Action::Enter { node: child, input })
    }

    fn on_function(&mut self, frame: FunctionFrame, up: Up) -> Result<Action<'a>, Fault> {
        let node = self.node(&frame.position)?;
        match up {
            Up::Task { output, directive } => {
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
                let (output, directive) = self.done(node, output, &frame.input, directive)?;
                Ok(Action::Up(Up::Task { output, directive }))
            }
            Up::Fault(fault) => Ok(Action::Up(Up::Fault(fault))),
            Up::Seq(_) | Up::Released => {
                Err(internal(node.position(), "function received a list result"))
            }
        }
    }

    // ========================================
    // Finishing a task
    // ========================================

    /// Apply `output.as` and `export.as`, then return what the parent list sees.
    fn done(
        &mut self,
        node: &Node,
        raw_output: Value,
        input: &Value,
        directive: Directive,
    ) -> Result<(Value, Directive), Fault> {
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
            // `export.as` replaces `$context`. Accumulate with `$context + {...}`.
            self.state.context = exported;
        }
        Ok((output, directive))
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

    // ========================================
    // Workflow input and output
    // ========================================

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
                let vars = self.scope(None, &self.state.workflow_input, Some(&output));
                evaluate(source, &output, &vars).map_err(|message| Fault::expression("/", message))
            }
            None => Ok(output),
        }
    }

    // ========================================
    // Retry policy
    // ========================================

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

    /// `limit.duration` bounds how long retries may continue.
    ///
    /// The window starts when the first retry pause (attempt 0) was first
    /// observed. That instant is the task start while that pause is pending,
    /// and is copied onto the try frame when the pause is released. Until it
    /// is known, the window stays open.
    fn retry_window_open(&self, node: &Node, input: &Value) -> Result<bool, Fault> {
        let Some(after) = self.retry_limit_duration(node, input, "/limit/duration")? else {
            return Ok(true);
        };
        let Some(Frame::Try(frame)) = self.state.frames.last() else {
            return Ok(true);
        };
        let started = frame.retry_started.or_else(|| {
            (frame.phase == TryPhase::Backoff && frame.attempt == 0)
                .then_some(self.state.task_started)
                .flatten()
        });
        Ok(!self.deadline_reached(started, after))
    }

    fn retry_limit_duration(
        &self,
        node: &Node,
        input: &Value,
        pointer: &str,
    ) -> Result<Option<Duration>, Fault> {
        let Some(policy) = self.retry_policy(node)? else {
            return Ok(None);
        };
        let Some(value) = policy.pointer(pointer) else {
            return Ok(None);
        };
        let millis = self.duration_of(value, node.position(), Some(node), input)?;
        Ok(Some(Duration::from_millis(millis)))
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

    // ========================================
    // Deadlines
    // ========================================

    fn deadline_reached(&self, started: Option<Timestamp>, after: Duration) -> bool {
        match (self.now, started) {
            (Some(now), Some(started)) => now >= started.saturating_add(after),
            _ => false,
        }
    }

    fn workflow_timeout(&self) -> Result<(), Fault> {
        let timeout = self.document.get("timeout");
        let Some(after) = self.timeout_after(timeout, "/", None, &self.state.workflow_input)?
        else {
            return Ok(());
        };
        if self.deadline_reached(self.state.workflow_started, after) {
            return Err(Fault::timeout(
                "/",
                format!("timed out after {}ms", after.as_millis()),
            ));
        }
        Ok(())
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

    // ========================================
    // Expressions and lookups
    // ========================================

    fn node(&self, position: &str) -> Result<&'a Node, Fault> {
        self.nodes.get(position).copied().ok_or_else(|| {
            Fault::runtime(
                position,
                "continuation refers to a node missing from the definition",
            )
        })
    }

    fn execution_key(&self) -> ExecutionKey {
        ExecutionKey::new(
            self.state
                .frames
                .iter()
                .filter_map(Frame::counter)
                .collect::<Vec<_>>(),
        )
    }

    fn scope(
        &self,
        node: Option<&Node>,
        input: &Value,
        output: Option<&Value>,
    ) -> BTreeMap<String, Value> {
        let mut map = BTreeMap::new();
        map.insert(
            "context".to_string(),
            Value::Object(self.state.context.clone()),
        );
        map.insert("input".to_string(), input.clone());
        map.insert(
            "workflow".to_string(),
            serde_json::json!({ "input": self.state.workflow_input }),
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
        let mut bindings = Map::new();
        for frame in &self.state.frames {
            frame.bindings(&mut bindings);
        }
        map.extend(bindings);
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

/// Static `listen.to` filters. Expressions, `correlate`, and `until` are not supported yet.
fn listen_spec(node: &Node) -> Result<ListenSpec, Fault> {
    let position = node.position();
    let unsupported =
        |what: &str| Fault::configuration(position, format!("listen {what} is not supported"));
    let listen = node
        .body()
        .get("listen")
        .and_then(Value::as_object)
        .ok_or_else(|| Fault::runtime(position, "listen task is missing `listen`"))?;
    let read = match listen.get("read") {
        None => "data".to_string(),
        Some(Value::String(mode)) if matches!(mode.as_str(), "data" | "envelope" | "raw") => {
            mode.clone()
        }
        Some(_) => {
            return Err(Fault::runtime(
                position,
                "listen.read must be data, envelope, or raw",
            ));
        }
    };
    let to = listen
        .get("to")
        .and_then(Value::as_object)
        .ok_or_else(|| Fault::runtime(position, "listen.to must be an object"))?;
    if to.contains_key("until") {
        return Err(unsupported("to.until"));
    }
    let mut chosen = None;
    for (key, strategy) in [
        ("one", ListenStrategy::One),
        ("any", ListenStrategy::Any),
        ("all", ListenStrategy::All),
    ] {
        if let Some(value) = to.get(key) {
            if chosen.is_some() {
                return Err(Fault::runtime(
                    position,
                    "listen.to must set only one of one, any, or all",
                ));
            }
            chosen = Some((key, strategy, value));
        }
    }
    let Some((key, strategy, value)) = chosen else {
        return Err(Fault::runtime(
            position,
            "listen.to must set one of one, any, or all",
        ));
    };
    let raw_filters: Vec<&Value> = match (strategy, value) {
        (ListenStrategy::One, Value::Object(_)) => vec![value],
        (ListenStrategy::Any | ListenStrategy::All, Value::Array(items)) => items.iter().collect(),
        _ => {
            return Err(Fault::runtime(
                position,
                format!("listen.to.{key} has the wrong shape"),
            ));
        }
    };
    let mut filters = Vec::with_capacity(raw_filters.len());
    for raw in raw_filters {
        let filter = raw
            .as_object()
            .ok_or_else(|| Fault::runtime(position, "listen filter must be an object"))?;
        if filter.contains_key("correlate") {
            return Err(unsupported("correlate"));
        }
        let with = filter
            .get("with")
            .and_then(Value::as_object)
            .ok_or_else(|| Fault::runtime(position, "listen filter must have `with`"))?;
        let mut attributes = BTreeMap::new();
        for (name, value) in with {
            let Value::String(text) = value else {
                return Err(unsupported("filters with non-string attributes"));
            };
            if runtime_expression(text).is_some() {
                return Err(unsupported("expression filters"));
            }
            attributes.insert(name.clone(), text.clone());
        }
        filters.push(EventFilter { attributes });
    }
    Ok(ListenSpec {
        strategy,
        filters,
        read,
    })
}

/// A `try` node's children are the `try` list followed by the `catch.do` list.
fn try_bodies(node: &Node) -> (&[Node], &[Node]) {
    let prefix = format!("{}/catch/", node.position());
    let catch_at = node
        .children()
        .iter()
        .position(|child| child.position().starts_with(&prefix))
        .unwrap_or(node.children().len());
    node.children().split_at(catch_at)
}

fn flow_directive(flow: Flow, default: Directive) -> (Value, Directive) {
    match flow {
        Flow::Finished(value) | Flow::Exited(value) => (value, default),
        Flow::Ended(value) => (value, Directive::End),
    }
}

fn internal(position: &str, detail: &str) -> Fault {
    Fault::runtime(position, format!("internal: {detail}"))
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
