//! Resolve a `run: workflow` block into the child the parent commit spawns.
//!
//! The child is an ordinary instance: `parent.branch` is `None`, and its
//! definition key is fixed here, through [`DefinitionSource::latest`]. The
//! parent's continuation does not store that definition. When the child
//! reaches a terminal status, the parent is released the same way a join is:
//! the next claim re-evaluates from `children` and resumes with the child's
//! output or its fault. `await: false` does not wait; the parent continues
//! with the child id as soon as the child row exists.

use serde_json::Value;
use tideloom_core::BlockDetail;
use tideloom_core::ChildRef;
use tideloom_core::Fault;
use tideloom_core::Resume;

use crate::DefinitionKey;
use crate::DefinitionSource;
use crate::Instance;
use crate::InstanceId;
use crate::ParentRef;
use crate::SpawnChild;
use crate::Status;
use crate::StoreError;

/// What a `run: workflow` block does to its parent.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ChildStart {
    /// Create the child and park the parent on `Waiting(Child)`.
    Await {
        /// The child row, created in the same commit.
        child: SpawnChild,
    },
    /// Create the child and continue the parent now.
    ///
    /// The resume is [`Resume::Output`] of the child id. A child workflow is
    /// effectful, so `Release` would be a contract violation.
    Fire {
        /// The child row, created in the same commit.
        child: SpawnChild,
        /// What the parent resumes with.
        resume: Resume,
    },
    /// The target is not published. The parent faults; nothing is spawned.
    ///
    /// This is a configuration fault, not a runtime one: the workflow names a
    /// definition that does not exist. A `try` around the task can catch it.
    Unpublished {
        /// The fault the parent resumes with.
        fault: Fault,
    },
}

/// Resolve `detail` against `definitions` and build the child to spawn.
///
/// `parent` is the instance that reached the `run: workflow` task. The child
/// id is chosen here so an `await: false` parent can resume with it before
/// the commit lands. A missing definition does not spawn anything.
///
/// # Errors
///
/// A definition lookup that fails for a reason other than "not published"
/// (a backend error) is returned as that [`StoreError`].
pub(crate) async fn start(
    definitions: &dyn DefinitionSource,
    parent: &InstanceId,
    position: &str,
    detail: &BlockDetail,
) -> Result<ChildStart, StoreError> {
    let BlockDetail::Child(child) = detail else {
        return Ok(ChildStart::Unpublished {
            fault: Fault::runtime(position, "a child block has no workflow target"),
        });
    };
    let key = match definitions
        .latest(&child.namespace, &child.name, &child.version)
        .await
    {
        Ok((key, _)) => key,
        Err(StoreError::Backend(detail)) if unpublished(&detail) => {
            return Ok(ChildStart::Unpublished {
                fault: Fault::configuration(
                    position,
                    format!(
                        "workflow {}/{}@{} is not published",
                        child.namespace, child.name, child.version
                    ),
                ),
            });
        }
        Err(error) => return Err(error),
    };
    Ok(planned(parent, key, child))
}

fn planned(parent: &InstanceId, definition: DefinitionKey, child: &ChildRef) -> ChildStart {
    let id = InstanceId::new();
    let spawned = SpawnChild {
        id: id.clone(),
        definition,
        parent: ParentRef {
            id: parent.clone(),
            branch: None,
        },
        input: child.input.clone(),
        // A child workflow starts at its own root. A fork branch is the
        // caller that sets `root_task`.
        root_task: None,
    };
    if child.await_result {
        ChildStart::Await { child: spawned }
    } else {
        ChildStart::Fire {
            child: spawned,
            resume: Resume::Output(Value::String(id.to_string())),
        }
    }
}

/// Recompute a waiting `run: workflow` from the child rows.
///
/// `children` is the store's order. Fork branches (`branch: Some`) are
/// ignored, so a workflow child of a branch does not look like a join. The
/// parent keeps waiting until that one child is terminal. A completed child
/// resumes the parent with [`Resume::Output`]; a faulted or cancelled child
/// resumes it with [`Resume::Fault`], which a surrounding `try` can catch.
/// A fork join does not: its fault is the parent's status. A child workflow
/// is an ordinary effectful task, so its fault is not a boundary.
#[must_use]
pub(crate) fn decide(children: &[Instance]) -> ChildDecision {
    let workflows: Vec<&Instance> = children
        .iter()
        .filter(|child| {
            child
                .parent
                .as_ref()
                .is_some_and(|parent| parent.branch.is_none())
        })
        .collect();
    // One `run: workflow` task spawns one child. A later task in the same
    // parent spawns another only after this one has been released, so the
    // open child is the one whose branch index is `None` and that is not yet
    // accounted for. The store orders those after every fork branch, then by
    // creation, so the latest is the one this pause is waiting on.
    let Some(child) = workflows.last() else {
        return ChildDecision::Pending;
    };
    match child.status {
        Status::Completed => ChildDecision::Output {
            output: child.output.clone().unwrap_or(Value::Null),
        },
        Status::Faulted => ChildDecision::Fault {
            fault: child.fault.clone().unwrap_or_else(|| missing_fault(child)),
        },
        Status::Cancelled => ChildDecision::Fault {
            fault: Fault::runtime(
                child.id.as_str(),
                "the child workflow was cancelled before it completed",
            ),
        },
        Status::Runnable | Status::Running | Status::Waiting(_) => ChildDecision::Pending,
    }
}

/// What a waiting parent should do with its child workflow.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ChildDecision {
    /// The child has not finished. Keep waiting.
    Pending,
    /// The child completed. Resume the parent with its output.
    Output {
        /// The child workflow output.
        output: Value,
    },
    /// The child failed. Resume the parent with this fault.
    Fault {
        /// The child fault, unchanged when the child recorded one.
        fault: Fault,
    },
}

fn missing_fault(instance: &Instance) -> Fault {
    Fault::runtime(
        instance.id.as_str(),
        "a faulted child workflow recorded no fault",
    )
}

/// `MemoryDefinitions::latest` reports a missing identity as a backend error
/// with this shape. The trait has no `NotFound`, and MR-6 does not add one.
fn unpublished(detail: &str) -> bool {
    detail.contains("is not published")
}
