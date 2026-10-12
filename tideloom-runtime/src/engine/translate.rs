//! Turn an interpreter [`Outcome`] into the [`Commit`] a step applies.
//!
//! Activity outcomes are applied in the step loop before they get here, so a
//! block that reaches `translate` is a pause the engine cannot finish inline:
//! a timer, a retry backoff, a join, a child workflow, or a listen.

use std::time::Duration;

use serde_json::Value;
use tideloom_core::Block;
use tideloom_core::BlockDetail;
use tideloom_core::Continuation;
use tideloom_core::Fault;
use tideloom_core::Outcome;
use tideloom_core::Pause;
use tideloom_core::Timestamp;

use crate::Commit;
use crate::DefinitionKey;
use crate::EngineError;
use crate::InstanceId;
use crate::ParentRef;
use crate::SpawnChild;
use crate::Status;
use crate::WaitReason;

/// What `translate` decided, before the step fills in the CAS guard.
pub struct Translated {
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
    /// Children created in the same commit. Only a join fills this.
    pub spawn_children: Vec<SpawnChild>,
}

/// Translate `outcome` for `instance`.
///
/// `definition` is the key the instance was created with. A join's branches
/// run that same tree, started at each branch position. `now` is the single
/// clock reading of the step. A timer's `wake_at` is `now + after`: the
/// continuation's task start is the join's, not the wait's, and using it
/// would make a wait after a join due in the past. A retry uses the delay
/// the interpreter already computed, added to `now` for the same reason.
///
/// # Errors
///
/// A continuation that will not serialize is an [`EngineError::Continuation`].
/// A pause the engine cannot act on yet (a child workflow, a listen) is an
/// [`EngineError::Unsupported`]: those waits belong to later milestone work,
/// and treating them as a timer would lose the subscription.
pub fn translate(
    instance: &InstanceId,
    expected_version: u64,
    definition: &DefinitionKey,
    outcome: Outcome,
    now: Timestamp,
) -> Result<Commit, EngineError> {
    let translated = match outcome {
        Outcome::Completed { output, .. } => Translated {
            status: Status::Completed,
            continuation: None,
            wake_at: None,
            output: Some(output),
            fault: None,
            spawn_children: Vec::new(),
        },
        Outcome::Faulted { fault, .. } => Translated {
            status: Status::Faulted,
            continuation: None,
            wake_at: None,
            output: None,
            fault: Some(fault),
            spawn_children: Vec::new(),
        },
        Outcome::Blocked {
            block,
            continuation,
        } => translate_block(instance, definition, &block, &continuation, now)?,
    };
    let mut commit = Commit::new(instance.clone(), expected_version, translated.status, now);
    commit.continuation = translated.continuation;
    commit.wake_at = translated.wake_at;
    commit.output = translated.output;
    commit.fault = translated.fault;
    commit.spawn_children = translated.spawn_children;
    Ok(commit)
}

fn translate_block(
    instance: &InstanceId,
    definition: &DefinitionKey,
    block: &Block,
    continuation: &Continuation,
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
                Some(add_duration(now, after)),
                Vec::new(),
            )
        }
        Pause::Retry { delay, .. } => (
            Status::Waiting(WaitReason::Timer),
            Some(add_duration(now, delay)),
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
                    // The position rides along so `start_at` knows which task
                    // is the branch root. `branch_input` strips it before the
                    // branch evaluates anything.
                    input: super::step::branch_input(block.input(), &branch.position),
                })
                .collect();
            (Status::Waiting(WaitReason::Join), None, children)
        }
        // `claim` only returns `Runnable` and a due `Waiting(Timer)`. An
        // activity that this step did not run — a fresh branch, so a compete
        // winner in the same pass can cancel it first — waits as a due timer
        // and is claimed again immediately. `Waiting(Activity)` would never
        // be claimed back.
        Pause::Activity => (Status::Waiting(WaitReason::Timer), Some(now), Vec::new()),
        Pause::Child => {
            return Err(EngineError::Unsupported(format!(
                "run: workflow at {} is not handled yet",
                block.position()
            )));
        }
        Pause::Events => {
            return Err(EngineError::Unsupported(format!(
                "listen at {} is not handled yet",
                block.position()
            )));
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

fn add_duration(start: Timestamp, delay: Duration) -> Timestamp {
    let extra = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
    Timestamp::from_millis(start.millis().saturating_add(extra))
}

/// Serialize a continuation. The store keeps the bytes and never parses them.
pub fn encode(instance: &InstanceId, continuation: &Continuation) -> Result<Vec<u8>, EngineError> {
    serde_json::to_vec(continuation).map_err(|error| EngineError::Continuation {
        instance: instance.to_string(),
        detail: error.to_string(),
    })
}

/// Decode a stored continuation.
pub fn decode(instance: &InstanceId, bytes: &[u8]) -> Result<Continuation, EngineError> {
    serde_json::from_slice(bytes).map_err(|error| EngineError::Continuation {
        instance: instance.to_string(),
        detail: error.to_string(),
    })
}
