use tideloom_core::BlockDetail;
use tideloom_core::EventFilter;
use tideloom_core::ListenSpec;
use tideloom_core::ListenStrategy;

use crate::CloudEvent;
use crate::EventSource;
use crate::InstanceId;

/// Subscribe `instance` to `spec` and park it on `Waiting(Events)`.
///
/// Called from the Events branch of `translate`. Idempotent: a repeated
/// subscribe keeps events already in the inbox.
pub(crate) async fn subscribe(
    events: &dyn EventSource,
    instance: &InstanceId,
    spec: ListenSpec,
) -> Result<(), crate::EventError> {
    events.subscribe(instance, spec).await
}

/// What a claimed `Waiting(Events)` instance should do with its inbox.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ListenStep {
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
pub(crate) fn step(detail: &BlockDetail, inbox: &[CloudEvent]) -> ListenStep {
    let BlockDetail::Listen(spec) = detail else {
        return ListenStep::Wait;
    };
    match release(spec, inbox) {
        Some(released) => ListenStep::Release(released),
        None => ListenStep::Wait,
    }
}

/// Whether `inbox` already satisfies `spec`.
///
/// `One` and `Any` need one matching event. `All` needs one event for every
/// filter. Events are not consumed. Matching is string equality on CloudEvents
/// context attributes; `data` is not a filter.
#[must_use]
pub(crate) fn satisfied(spec: &ListenSpec, inbox: &[CloudEvent]) -> bool {
    match spec.strategy {
        ListenStrategy::One | ListenStrategy::Any => inbox
            .iter()
            .any(|event| spec.filters.iter().any(|filter| matches(filter, event))),
        ListenStrategy::All => spec
            .filters
            .iter()
            .all(|filter| inbox.iter().any(|event| matches(filter, event))),
    }
}

/// The events a `Release` should carry, oldest first, and the ids to ack.
///
/// `None` when the inbox does not yet satisfy the strategy. `One` and `Any`
/// release the first match. `All` releases the first match of each filter, in
/// filter order. `read: data` yields each event's `data`; `envelope` and `raw`
/// yield the whole event. The [`Resume::Release`](tideloom_core::Resume::Release)
/// value is the caller's conversion of `events`, not this return.
#[must_use]
pub(crate) fn release(spec: &ListenSpec, inbox: &[CloudEvent]) -> Option<ListenRelease> {
    let chosen = match spec.strategy {
        ListenStrategy::One | ListenStrategy::Any => {
            let event = inbox
                .iter()
                .find(|event| spec.filters.iter().any(|filter| matches(filter, event)))?;
            vec![event]
        }
        ListenStrategy::All => {
            let mut chosen = Vec::with_capacity(spec.filters.len());
            for filter in &spec.filters {
                let event = inbox.iter().find(|event| matches(filter, event))?;
                chosen.push(event);
            }
            chosen
        }
    };
    let ids = chosen.iter().map(|event| event.id.clone()).collect();
    let events = chosen
        .into_iter()
        .map(|event| read_event(spec, event))
        .collect();
    Some(ListenRelease { events, ids })
}

/// What a satisfied listen releases, and which inbox ids that consumes.
///
/// Crate-visible. Callers outside the engine convert `events` into
/// `Resume::Release` themselves; this type is not re-exported.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ListenRelease {
    /// The values handed to `Resume::Release`, in strategy order.
    pub events: Vec<serde_json::Value>,
    /// Inbox ids to [`EventSource::ack`] after the commit lands.
    pub ids: Vec<String>,
}

fn matches(filter: &EventFilter, event: &CloudEvent) -> bool {
    filter
        .attributes
        .iter()
        .all(|(name, expected)| event.attribute(name) == Some(expected.as_str()))
}

fn read_event(spec: &ListenSpec, event: &CloudEvent) -> serde_json::Value {
    if spec.read == "data" {
        event.data.clone()
    } else {
        serde_json::json!({
            "id": event.id,
            "type": event.r#type,
            "source": event.source,
            "subject": event.subject,
            "time": event.time,
            "data": event.data,
        })
    }
}

/// Read the inbox and decide whether this claim can release the listen.
pub(crate) async fn after_claim(
    events: &dyn EventSource,
    instance: &InstanceId,
    detail: &BlockDetail,
) -> Result<ListenStep, crate::EventError> {
    let inbox = events.inbox(instance).await?;
    Ok(step(detail, &inbox))
}

/// Remove consumed events. Call this only after the `Release` commit lands.
pub(crate) async fn ack_after_commit(
    events: &dyn EventSource,
    instance: &InstanceId,
    ids: &[String],
) -> Result<(), crate::EventError> {
    events.ack(instance, ids).await
}
