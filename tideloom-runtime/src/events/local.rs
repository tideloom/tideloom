use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use tideloom_core::EventFilter;
use tideloom_core::ListenSpec;
use tideloom_core::ListenStrategy;
use tokio::sync::Mutex;

use crate::CloudEvent;
use crate::EventError;
use crate::EventSink;
use crate::EventSource;
use crate::Hint;
use crate::InstanceId;
use crate::Wakeup;

/// In-process [`EventSink`] and [`EventSource`] for one process.
///
/// A published event is copied into every inbox whose [`ListenSpec`] matches
/// it, including events that arrive before the listener is claimed again. The
/// inbox outlives the claim; only [`EventSource::ack`] removes events.
/// Subscriptions are indexed by their static attributes so a publish checks
/// only listeners that share an attribute value with the event. The match
/// itself is still full string equality on every filter attribute.
///
/// Matching is string equality on CloudEvents context attributes. `data` is
/// not a filter. `foreach`, `until`, and correlation are rejected by the core
/// before a spec reaches this bus.
pub struct LocalEventBus {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    wakeup: Option<Arc<dyn Wakeup>>,
}

#[derive(Default)]
struct State {
    subscriptions: HashMap<InstanceId, Subscription>,
    index: HashMap<IndexKey, Vec<InstanceId>>,
    /// Recent events, oldest first. A subscribe replays the ones that match,
    /// so an event published before the listen task runs is not lost. The
    /// bound is [`RETAINED`]; older events are dropped.
    retained: VecDeque<CloudEvent>,
}

/// How many recent events a subscribe can still see.
const RETAINED: usize = 1024;

struct Subscription {
    spec: ListenSpec,
    inbox: Vec<CloudEvent>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct IndexKey {
    name: String,
    value: String,
}

impl LocalEventBus {
    /// A closed bus. Published events reach inboxes and nowhere else.
    #[must_use]
    pub fn new() -> Self {
        Self::with_wakeup(None)
    }

    /// A bus that also hints `wakeup` when an event lands in an inbox.
    ///
    /// The hint is [`Hint::Event`] with the event id. Losing it loses latency,
    /// not the event: the inbox still holds it for the next claim.
    #[must_use]
    pub fn connected(wakeup: Arc<dyn Wakeup>) -> Self {
        Self::with_wakeup(Some(wakeup))
    }

    fn with_wakeup(wakeup: Option<Arc<dyn Wakeup>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                wakeup,
            }),
        }
    }

    /// Whether `inbox` already satisfies `spec`.
    ///
    /// `One` and `Any` need one matching event. `All` needs one event for
    /// every filter. Events are not consumed.
    #[must_use]
    pub fn satisfied(spec: &ListenSpec, inbox: &[CloudEvent]) -> bool {
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
    /// `None` when the inbox does not yet satisfy the strategy. `One` and
    /// `Any` release the first match. `All` releases the first match of each
    /// filter, in filter order. `read: data` yields each event's `data`;
    /// `envelope` and `raw` yield the whole event.
    #[must_use]
    pub fn release(spec: &ListenSpec, inbox: &[CloudEvent]) -> Option<ListenRelease> {
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
}

impl Default for LocalEventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// What a satisfied listen releases, and which inbox ids that consumes.
#[derive(Clone, Debug, PartialEq)]
pub struct ListenRelease {
    /// The array handed to `Resume::Release`, in strategy order.
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

fn index_keys(spec: &ListenSpec) -> Vec<IndexKey> {
    let mut keys = Vec::new();
    for filter in &spec.filters {
        for (name, value) in &filter.attributes {
            keys.push(IndexKey {
                name: name.clone(),
                value: value.clone(),
            });
        }
    }
    keys
}

fn unindex(state: &mut State, instance: &InstanceId, spec: &ListenSpec) {
    for key in index_keys(spec) {
        let Some(instances) = state.index.get_mut(&key) else {
            continue;
        };
        instances.retain(|candidate| candidate != instance);
        if instances.is_empty() {
            state.index.remove(&key);
        }
    }
}

#[async_trait]
impl EventSink for LocalEventBus {
    async fn publish(&self, event: CloudEvent) -> Result<(), EventError> {
        let mut landed = Vec::new();
        {
            let mut state = self.inner.state.lock().await;
            // An event can match a filter on an attribute this event does not
            // carry only by failing that filter, so the index is complete:
            // every subscription has at least one attribute, and an event is
            // a candidate when it shares one. An event with no attributes at
            // all cannot match, and is not scanned.
            let mut candidates: BTreeMap<InstanceId, ()> = BTreeMap::new();
            for (name, value) in event_attributes(&event) {
                let key = IndexKey { name, value };
                if let Some(instances) = state.index.get(&key) {
                    for instance in instances {
                        candidates.insert(instance.clone(), ());
                    }
                }
            }
            let targets: Vec<InstanceId> = candidates.into_keys().collect();
            for instance in targets {
                let Some(subscription) = state.subscriptions.get_mut(&instance) else {
                    continue;
                };
                if !subscription
                    .spec
                    .filters
                    .iter()
                    .any(|filter| matches(filter, &event))
                {
                    continue;
                }
                subscription.inbox.push(event.clone());
                landed.push(instance);
            }
            state.retained.push_back(event.clone());
            while state.retained.len() > RETAINED {
                state.retained.pop_front();
            }
        }
        if let Some(wakeup) = &self.inner.wakeup {
            for instance in landed {
                let _ = wakeup.notify(instance, Hint::Event(event.id.clone())).await;
            }
        }
        Ok(())
    }
}

fn event_attributes(event: &CloudEvent) -> Vec<(String, String)> {
    let mut attributes = vec![
        ("id".to_string(), event.id.clone()),
        ("type".to_string(), event.r#type.clone()),
        ("source".to_string(), event.source.clone()),
    ];
    if let Some(subject) = &event.subject {
        attributes.push(("subject".to_string(), subject.clone()));
    }
    if let Some(time) = &event.time {
        attributes.push(("time".to_string(), time.clone()));
    }
    attributes.extend(
        event
            .extensions
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    attributes
}

#[async_trait]
impl EventSource for LocalEventBus {
    async fn subscribe(&self, instance: &InstanceId, spec: ListenSpec) -> Result<(), EventError> {
        let mut state = self.inner.state.lock().await;
        if let Some(existing) = state.subscriptions.get(instance)
            && existing.spec == spec
        {
            return Ok(());
        }
        let mut inbox = if let Some(existing) = state.subscriptions.remove(instance) {
            unindex(&mut state, instance, &existing.spec);
            existing.inbox
        } else {
            Vec::new()
        };
        // Replay events published before this subscribe. An emit earlier in
        // the same workflow, or an event from another instance, is still here.
        for event in &state.retained {
            if inbox.iter().any(|held| held.id == event.id) {
                continue;
            }
            if spec.filters.iter().any(|filter| matches(filter, event)) {
                inbox.push(event.clone());
            }
        }
        // A repeated subscribe for a different spec keeps events already
        // collected. The index is rebuilt from the new spec.
        for key in index_keys(&spec) {
            state.index.entry(key).or_default().push(instance.clone());
        }
        state
            .subscriptions
            .insert(instance.clone(), Subscription { spec, inbox });
        Ok(())
    }

    async fn unsubscribe(&self, instance: &InstanceId) -> Result<(), EventError> {
        let mut state = self.inner.state.lock().await;
        if let Some(existing) = state.subscriptions.remove(instance) {
            unindex(&mut state, instance, &existing.spec);
        }
        Ok(())
    }

    async fn inbox(&self, instance: &InstanceId) -> Result<Vec<CloudEvent>, EventError> {
        let state = self.inner.state.lock().await;
        Ok(state
            .subscriptions
            .get(instance)
            .map(|subscription| subscription.inbox.clone())
            .unwrap_or_default())
    }

    async fn ack(&self, instance: &InstanceId, ids: &[String]) -> Result<(), EventError> {
        let mut state = self.inner.state.lock().await;
        if let Some(subscription) = state.subscriptions.get_mut(instance) {
            subscription
                .inbox
                .retain(|event| !ids.iter().any(|id| id == &event.id));
        }
        Ok(())
    }
}
