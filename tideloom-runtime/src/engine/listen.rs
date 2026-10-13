use tideloom_core::BlockDetail;
use tideloom_core::ListenSpec;

use crate::CloudEvent;
use crate::EventSource;
use crate::InstanceId;
use crate::ListenRelease;
use crate::LocalEventBus;

/// Subscribe `instance` to `spec` and park it on `Waiting(Events)`.
///
/// Called from the Events branch of `translate`. Idempotent: a repeated
/// subscribe keeps events already in the inbox.
pub async fn subscribe(
    events: &dyn EventSource,
    instance: &InstanceId,
    spec: ListenSpec,
) -> Result<(), crate::EventError> {
    events.subscribe(instance, spec).await
}

/// What a claimed `Waiting(Events)` instance should do with its inbox.
#[derive(Clone, Debug, PartialEq)]
pub enum ListenStep {
    /// The inbox satisfies the strategy. Resume with `Release(events)`, then
    /// ack `ids` only after `Engine::commit` succeeds.
    Release(ListenRelease),
    /// Keep waiting. The subscription stays; nothing is acked.
    Wait,
}

/// Decide a listen step from the block detail and the current inbox.
///
/// `One` and `Any` release on the first matching event. `All` releases only
/// when every filter has a match. A non-listen detail waits.
#[must_use]
pub fn step(detail: &BlockDetail, inbox: &[CloudEvent]) -> ListenStep {
    let BlockDetail::Listen(spec) = detail else {
        return ListenStep::Wait;
    };
    match LocalEventBus::release(spec, inbox) {
        Some(release) => ListenStep::Release(release),
        None => ListenStep::Wait,
    }
}

/// Read the inbox and decide whether this claim can release the listen.
pub async fn after_claim(
    events: &dyn EventSource,
    instance: &InstanceId,
    detail: &BlockDetail,
) -> Result<ListenStep, crate::EventError> {
    let inbox = events.inbox(instance).await?;
    Ok(step(detail, &inbox))
}

/// Remove consumed events. Call this only after the `Release` commit lands.
pub async fn ack_after_commit(
    events: &dyn EventSource,
    instance: &InstanceId,
    ids: &[String],
) -> Result<(), crate::EventError> {
    events.ack(instance, ids).await
}
