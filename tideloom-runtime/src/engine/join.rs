//! Re-evaluate a `fork` join from the child rows.
//!
//! The waiting condition is not stored. Whoever claims the parent reads
//! `children` and decides again. Two children finishing at once each produce
//! a commit against the same parent version; `apply` keeps one and the other
//! is a version conflict, which the engine drops.

use serde_json::Value;
use tideloom_core::Fault;

use crate::Instance;
use crate::InstanceId;
use crate::Status;

/// What a join should do, given the children it has right now.
#[derive(Clone, Debug, PartialEq)]
pub enum JoinDecision {
    /// Not every branch has reported. Keep waiting.
    Pending,
    /// Cooperative: every branch completed. The array is in branch order.
    /// Compete: the first completed branch, and the rest are cancelled.
    Release {
        /// What the fork task continues with.
        output: Value,
        /// Branches that are still running and must stop.
        cancel: Vec<InstanceId>,
    },
    /// The join failed. Remaining branches are cancelled.
    ///
    /// A fork is an error boundary: this fault is applied as the parent's
    /// status, not handed to a `try` outside the fork. The core already
    /// stopped at the join, so the outer `try` never sees the branch fault.
    Fault {
        /// The branch fault, unchanged.
        fault: Fault,
        /// Branches that are still running and must stop.
        cancel: Vec<InstanceId>,
    },
}

/// Recompute a join.
///
/// `children` must be the store's order: branch index ascending, then
/// creation time. A missing branch index sorts with the workflow children,
/// which this function ignores. `compete` selects the race.
///
/// Cooperative releases only when every branch is `Completed`, in branch
/// order. One `Faulted` branch faults the join and cancels the rest; a
/// `Cancelled` branch with no fault of its own is the same failure, because
/// a cooperative join cannot complete without it. Compete releases on the
/// first `Completed` branch and cancels the rest, and faults only when every
/// branch is `Faulted` or `Cancelled`.
#[must_use]
pub fn decide(children: &[Instance], compete: bool) -> JoinDecision {
    let branches: Vec<&Instance> = children
        .iter()
        .filter(|child| {
            child
                .parent
                .as_ref()
                .is_some_and(|parent| parent.branch.is_some())
        })
        .collect();
    if branches.is_empty() {
        return JoinDecision::Pending;
    }
    if compete {
        return compete_join(&branches);
    }
    cooperative(&branches)
}

fn cooperative(branches: &[&Instance]) -> JoinDecision {
    if let Some(failed) = branches
        .iter()
        .find(|child| child.status == Status::Faulted)
    {
        return JoinDecision::Fault {
            fault: failed
                .fault
                .clone()
                .unwrap_or_else(|| missing_fault(failed)),
            cancel: still_running(branches),
        };
    }
    if branches.iter().any(|child| !child.status.is_terminal()) {
        return JoinDecision::Pending;
    }
    if let Some(cancelled) = branches
        .iter()
        .find(|child| child.status == Status::Cancelled)
    {
        return JoinDecision::Fault {
            fault: Fault::runtime(
                cancelled.id.as_str(),
                "a cooperative branch was cancelled before it completed",
            ),
            cancel: Vec::new(),
        };
    }
    JoinDecision::Release {
        output: Value::Array(
            branches
                .iter()
                .map(|child| child.output.clone().unwrap_or(Value::Null))
                .collect(),
        ),
        cancel: Vec::new(),
    }
}

fn compete_join(branches: &[&Instance]) -> JoinDecision {
    if let Some(winner) = branches
        .iter()
        .find(|child| child.status == Status::Completed)
    {
        return JoinDecision::Release {
            output: winner.output.clone().unwrap_or(Value::Null),
            cancel: still_running(branches),
        };
    }
    if branches.iter().any(|child| !child.status.is_terminal()) {
        return JoinDecision::Pending;
    }
    let failed = branches
        .iter()
        .find(|child| child.status == Status::Faulted);
    let fault = failed
        .and_then(|child| child.fault.clone())
        .unwrap_or_else(|| {
            Fault::runtime(
                branches
                    .first()
                    .map(|child| child.id.as_str())
                    .unwrap_or("/"),
                "every competing branch failed",
            )
        });
    JoinDecision::Fault {
        fault,
        cancel: Vec::new(),
    }
}

fn still_running(branches: &[&Instance]) -> Vec<InstanceId> {
    branches
        .iter()
        .filter(|child| !child.status.is_terminal())
        .map(|child| child.id.clone())
        .collect()
}

fn missing_fault(instance: &Instance) -> Fault {
    Fault::runtime(instance.id.as_str(), "a faulted branch recorded no fault")
}
