//! Turn an interpreter [`Outcome`] into the [`Commit`] a step applies.
//!
//! Activity outcomes are applied in the step loop before they get here, so a
//! block that reaches `translate` is a pause the engine cannot finish inline:
//! a timer, a retry backoff, a join, a child workflow, or a listen. A child
//! workflow is resolved before this function runs: the step calls
//! [`prepare_child`] so `DefinitionSource::latest` can pin the child's key.

use std::time::Duration;

use serde_json::Value;
use tideloom_core::Block;
use tideloom_core::BlockDetail;
use tideloom_core::Continuation;
use tideloom_core::Definition;
use tideloom_core::Fault;
use tideloom_core::Outcome;
use tideloom_core::Pause;
use tideloom_core::Resume;
use tideloom_core::Timestamp;
use tideloom_core::WalkOptions;
use tideloom_core::resume_with;

use super::child::ChildStart;
use crate::Commit;
use crate::DefinitionKey;
use crate::DefinitionSource;
use crate::EngineError;
use crate::Instance;
use crate::InstanceId;
use crate::ParentRef;
use crate::SpawnChild;
use crate::Status;
use crate::StoreError;
use crate::WaitReason;

/// What `translate` decided, before the step fills in the CAS guard.
pub(crate) struct Translated {
    /// New status.
    pub status: Status,
    /// Serialized continuation, `None` when terminal.
    pub continuation: Option<Vec<u8>>,
    /// When a timer or a retry backoff may be claimed again.
    pub wake_at: Option<Timestamp>,
    /// Output when `Completed`.
    pub output: Option<Value>,
    /// Fault when `Faulted`.
    pub fault: Option<Fault>,
    /// Children created in the same commit: a join's branches, or a
    /// `run: workflow` child.
    pub spawn_children: Vec<SpawnChild>,
}

/// Translate `outcome` for `instance`.
///
/// `definition` is the key the instance was created with. A join's branches
/// run that same tree, started at each branch position. `now` is the single
/// clock reading of the step. A timer's `wake_at` is
/// `continuation.task_started() + after`, and a retry's is
/// `continuation.task_started() + delay` — the same instant [`Pause::retry_at`]
/// already computes. A missing task start falls back to `now`, which is how
/// the first observation sticks. A later tick of the same pause does not move
/// `wake_at`.
///
/// # Errors
///
/// A continuation that will not serialize is an [`EngineError::Continuation`].
/// A child workflow must already have been resolved by [`prepare_child`].
/// Reaching `Pause::Child` here means that step was skipped. `spawn` is the
/// child that resolution produced; other pauses pass an empty list.
pub(crate) fn translate(
    instance: &InstanceId,
    expected_version: u64,
    definition: &DefinitionKey,
    outcome: Outcome,
    spawn: Vec<SpawnChild>,
    now: Timestamp,
) -> Result<Commit, EngineError> {
    let translated = match outcome {
        Outcome::Completed { output, .. } => Translated {
            status: Status::Completed,
            continuation: None,
            wake_at: None,
            output: Some(output),
            fault: None,
            // `await: false` completes the parent in the same step that
            // creates the child. The child still has to be spawned.
            spawn_children: spawn,
        },
        Outcome::Faulted { fault, .. } => Translated {
            status: Status::Faulted,
            continuation: None,
            wake_at: None,
            output: None,
            fault: Some(fault),
            spawn_children: spawn,
        },
        Outcome::Blocked {
            block,
            continuation,
        } => translate_block(instance, definition, &block, &continuation, &spawn, now)?,
    };
    let mut commit = Commit::new(instance.clone(), expected_version, translated.status, now);
    commit.continuation = translated.continuation;
    commit.wake_at = translated.wake_at;
    commit.output = translated.output;
    commit.fault = translated.fault;
    commit.spawn_children = translated.spawn_children;
    Ok(commit)
}

/// An outcome plus the child a `run: workflow` resolution attached to it.
pub(crate) struct Prepared {
    /// What `translate` turns into a commit.
    pub outcome: Outcome,
    /// The child to create in that commit. Empty when the outcome spawned none.
    pub spawn: Vec<SpawnChild>,
}

/// Resolve a `run: workflow` block into the outcome `translate` should see.
///
/// `await: true` keeps the child pause and returns the child to spawn, so the
/// parent parks on `Waiting(Child)`. `await: false` resumes the parent with
/// the child id and still returns that child. An unpublished target becomes
/// [`Resume::Fault`], which a surrounding `try` can catch. A non-child outcome
/// is returned with an empty spawn list.
///
/// # Errors
///
/// A definition lookup that is not "not published" is an [`EngineError`].
pub(crate) async fn prepare_child(
    definitions: &dyn DefinitionSource,
    instance: &Instance,
    definition: &Definition,
    outcome: Outcome,
    now: Timestamp,
) -> Result<Prepared, EngineError> {
    let Outcome::Blocked {
        block,
        continuation,
    } = &outcome
    else {
        return Ok(Prepared {
            outcome,
            spawn: Vec::new(),
        });
    };
    if block.pause() != Pause::Child {
        return Ok(Prepared {
            outcome,
            spawn: Vec::new(),
        });
    }
    let started = super::child::start(definitions, &instance.id, block.position(), block.detail())
        .await
        .map_err(child_lookup)?;
    match started {
        ChildStart::Await { child } => Ok(Prepared {
            outcome,
            spawn: vec![child],
        }),
        ChildStart::Fire { child, resume } => {
            let resumed = resume_with(
                definition,
                continuation.clone(),
                resume,
                WalkOptions::new().with_now(now),
            );
            // The resume may itself stop on another child. Resolve that one
            // too, and keep this child in the same commit.
            let mut prepared = Box::pin(prepare_child(
                definitions,
                instance,
                definition,
                resumed,
                now,
            ))
            .await?;
            prepared.spawn.insert(0, child);
            Ok(prepared)
        }
        ChildStart::Unpublished { fault } => {
            let resumed = resume_with(
                definition,
                continuation.clone(),
                Resume::Fault(fault),
                WalkOptions::new().with_now(now),
            );
            // A caught configuration fault may continue into another child.
            Box::pin(prepare_child(
                definitions,
                instance,
                definition,
                resumed,
                now,
            ))
            .await
        }
    }
}

fn child_lookup(error: StoreError) -> EngineError {
    EngineError::Store(error)
}

fn translate_block(
    instance: &InstanceId,
    definition: &DefinitionKey,
    block: &Block,
    continuation: &Continuation,
    spawn: &[SpawnChild],
    now: Timestamp,
) -> Result<Translated, EngineError> {
    let (status, wake_at, spawn_children) = match block.pause() {
        Pause::Timer => {
            let after = match block.detail() {
                BlockDetail::Wait { after } => *after,
                _ => Duration::ZERO,
            };
            (
                Status::Waiting(WaitReason::Timer),
                Some(park_until(task_started(continuation, now), after)),
                Vec::new(),
            )
        }
        Pause::Retry { delay, .. } => (
            Status::Waiting(WaitReason::Timer),
            // `Pause::retry_at` is this sum. The caller used to pass `now`,
            // which moved the deadline every time the pause was reparked.
            Some(park_until(task_started(continuation, now), delay)),
            Vec::new(),
        ),
        Pause::Join { .. } => {
            let branches = match block.detail() {
                BlockDetail::Branches(branches) => branches,
                _ => {
                    return Err(EngineError::Continuation {
                        instance: instance.to_string(),
                        detail: "a join block has no branches".to_string(),
                    });
                }
            };
            let children = branches
                .iter()
                .enumerate()
                .map(|(index, branch)| SpawnChild {
                    id: InstanceId::new(),
                    definition: definition.clone(),
                    parent: ParentRef {
                        id: instance.clone(),
                        branch: Some(index),
                    },
                    // The fork input is the branch input. The position is a
                    // column, not a key hidden inside that value.
                    input: block.input().clone(),
                    root_task: Some(branch.position.clone()),
                })
                .collect();
            (Status::Waiting(WaitReason::Join), None, children)
        }
        Pause::Child => {
            if spawn.is_empty() {
                return Err(EngineError::Unsupported(format!(
                    "run: workflow at {} was not resolved before translate",
                    block.position()
                )));
            }
            (Status::Waiting(WaitReason::Child), None, spawn.to_vec())
        }
        // The step runs activities before `translate`. Reaching one here is a
        // runner contract violation, same as a child workflow that was not
        // resolved: parking it would hide the skipped executor behind a timer.
        Pause::Activity => {
            return Err(EngineError::Unsupported(format!(
                "activity at {} reached translate without running",
                block.position()
            )));
        }
        Pause::Events => {
            // The step subscribes before applying this commit, then flips the
            // instance to `Runnable` when the inbox already matches. While
            // it does not, the instance stays `Waiting(Events)`, which
            // `claim` does not return. A landed event is what makes it
            // claimable again; the signal is only the hint.
            if !matches!(block.detail(), BlockDetail::Listen(_)) {
                return Err(EngineError::Continuation {
                    instance: instance.to_string(),
                    detail: "a listen block has no subscription".to_string(),
                });
            }
            // Not claimable until the inbox matches. The step flips a
            // satisfied listen to a due timer before this commit lands.
            (Status::Waiting(WaitReason::Events), None, Vec::new())
        }
    };
    Ok(Translated {
        status,
        continuation: Some(encode(instance, continuation)?),
        wake_at,
        output: None,
        fault: None,
        spawn_children,
    })
}

/// When the pending pause was first observed.
///
/// The interpreter stamps `task_started` on the block it just built, which is
/// this step's `now`. A tick of the same pause keeps the stored instant, so
/// the caller that recorded the first observation is what makes a repark
/// stable. A missing stamp falls back to `now`.
fn task_started(continuation: &Continuation, now: Timestamp) -> Timestamp {
    continuation.task_started().unwrap_or(now)
}

/// When a timer or a retry may be claimed again.
///
/// `claim` treats `wake_at <= now` as due. A zero duration is a real stop, so
/// it parks one millisecond after the task start; otherwise the same pass
/// would claim it again and never idle. A positive duration is added to the
/// task start as written. The anchor is the first observation, not the clock
/// reading of a later tick.
fn park_until(started: Timestamp, delay: Duration) -> Timestamp {
    let delay = if delay.is_zero() {
        Duration::from_millis(1)
    } else {
        delay
    };
    add_duration(started, delay)
}

fn add_duration(start: Timestamp, delay: Duration) -> Timestamp {
    let extra = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
    Timestamp::from_millis(start.millis().saturating_add(extra))
}

/// Serialize a continuation. The store keeps the bytes and never parses them.
pub(crate) fn encode(
    instance: &InstanceId,
    continuation: &Continuation,
) -> Result<Vec<u8>, EngineError> {
    serde_json::to_vec(continuation).map_err(|error| EngineError::Continuation {
        instance: instance.to_string(),
        detail: error.to_string(),
    })
}

/// Decode a stored continuation.
pub(crate) fn decode(instance: &InstanceId, bytes: &[u8]) -> Result<Continuation, EngineError> {
    serde_json::from_slice(bytes).map_err(|error| EngineError::Continuation {
        instance: instance.to_string(),
        detail: error.to_string(),
    })
}
