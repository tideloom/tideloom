use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tideloom_core::EventFilter;
use tideloom_core::ListenSpec;
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
/// A published event is copied into every inbox whose [`ListenSpec`] already
/// matches it. An event published before [`EventSource::subscribe`] is lost;
/// this bus does not keep a log to replay into a later inbox. The inbox
/// outlives the claim; only [`EventSource::ack`] removes events. Subscriptions
/// are indexed by their static attributes so a publish checks only listeners
/// that share an attribute value with the event. The match itself is still
/// full string equality on every filter attribute.
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
}

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
}

impl Default for LocalEventBus {
    fn default() -> Self {
        Self::new()
    }
}

fn matches(filter: &EventFilter, event: &CloudEvent) -> bool {
    filter
        .attributes
        .iter()
        .all(|(name, expected)| event.attribute(name) == Some(expected.as_str()))
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
    async fn publish(&self, event: CloudEvent) -> Result<Vec<InstanceId>, EventError> {
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
        }
        if let Some(wakeup) = &self.inner.wakeup {
            for instance in &landed {
                let _ = wakeup
                    .notify(instance.clone(), Hint::Event(event.id.clone()))
                    .await;
            }
        }
        Ok(landed)
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
        let inbox = if let Some(existing) = state.subscriptions.remove(instance) {
            unindex(&mut state, instance, &existing.spec);
            existing.inbox
        } else {
            Vec::new()
        };
        // A repeated subscribe for a different spec keeps events already
        // collected. Nothing published before this subscribe is copied in.
        // The index is rebuilt from the new spec.
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
