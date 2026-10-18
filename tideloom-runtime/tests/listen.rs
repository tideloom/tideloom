//! `emit` publishes into `LocalEventBus`; `listen` collects in a per-instance
//! inbox and releases only when the strategy is satisfied.
//!
//! The bus tests cover matching, `all`, early arrival, and `read: envelope`.
//! The engine test drives the same path through `run_once`: subscribe on the
//! Events pause, claim the due listen, `Resume::Release`, then ack.

use std::num::NonZeroUsize;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::json;
use tideloom_core::BlockDetail;
use tideloom_core::Definition;
use tideloom_core::EventFilter;
use tideloom_core::ListenSpec;
use tideloom_core::ListenStrategy;
use tideloom_core::Outcome;
use tideloom_core::TaskKey;
use tideloom_runtime::ActivityExecutor;
use tideloom_runtime::Clock;
use tideloom_runtime::CloudEvent;
use tideloom_runtime::DefinitionKey;
use tideloom_runtime::EmitExecutor;
use tideloom_runtime::Engine;
use tideloom_runtime::EngineParts;
use tideloom_runtime::EventSink;
use tideloom_runtime::EventSource;
use tideloom_runtime::Hint;
use tideloom_runtime::Instance;
use tideloom_runtime::InstanceId;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::LocalEventBus;
use tideloom_runtime::LocalWakeup;
use tideloom_runtime::ManualClock;
use tideloom_runtime::MemoryDefinitions;
use tideloom_runtime::MemoryStore;
use tideloom_runtime::Status;
use tideloom_runtime::TaskExecution;
use tideloom_runtime::TaskKind;
use tideloom_runtime::Wakeup;
use tideloom_runtime::WorkerId;
use tideloom_runtime::engine::listen;
use tideloom_runtime::engine::listen::ListenStep;

fn one(type_name: &str) -> ListenSpec {
    ListenSpec {
        strategy: ListenStrategy::One,
        filters: vec![filter(&[("type", type_name)])],
        read: "data".to_string(),
    }
}

fn filter(pairs: &[(&str, &str)]) -> EventFilter {
    EventFilter {
        attributes: pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect(),
    }
}

fn event(id: &str, type_name: &str, data: serde_json::Value) -> CloudEvent {
    CloudEvent {
        id: id.to_string(),
        r#type: type_name.to_string(),
        source: "https://example.test/orders".to_string(),
        subject: None,
        time: None,
        extensions: std::collections::BTreeMap::new(),
        data,
    }
}

fn detail(spec: ListenSpec) -> BlockDetail {
    BlockDetail::Listen(spec)
}

fn execution(
    definition: &Definition,
    key: DefinitionKey,
    input: serde_json::Value,
) -> TaskExecution {
    let Outcome::Blocked {
        block,
        continuation,
    } = tideloom_core::start(definition, &input)
    else {
        panic!("the emit is a block, got a finished walk");
    };
    execution_from(key, input, block.detail().clone(), continuation)
}

fn execution_with(
    definition: DefinitionKey,
    input: serde_json::Value,
    detail: tideloom_core::BlockDetail,
    continuation: tideloom_core::Continuation,
) -> TaskExecution {
    execution_from(definition, input, detail, continuation)
}

fn execution_from(
    definition: DefinitionKey,
    input: serde_json::Value,
    detail: tideloom_core::BlockDetail,
    continuation: tideloom_core::Continuation,
) -> TaskExecution {
    let key = TaskKey::new("/do/0/notify", tideloom_core::ExecutionKey::new(vec![]));
    TaskExecution {
        instance: InstanceId::from_string("emitter"),
        definition,
        key,
        kind: TaskKind::Emit,
        attempt: 0,
        input,
        context: json!({}),
        workflow_input: json!({}),
        detail,
        continuation,
    }
}

#[tokio::test]
async fn the_engine_releases_a_listen_after_an_emit() {
    let yaml = r#"
document:
  dsl: '1.0.0'
  namespace: default
  name: paint
  version: '0.1.0'
do:
  - notify:
      emit:
        event:
          with:
            source: https://example.test/orders
            type: io.example.paid
            data:
              order: ${ .order }
  - hear:
      listen:
        to:
          one:
            with:
              type: io.example.paid
  - after:
      set:
        heard: true
"#;
    let definition = Definition::from_yaml(yaml).unwrap_or_else(|error| panic!("{error}"));
    let definitions = Arc::new(MemoryDefinitions::new());
    let key = definitions.publish(definition);
    let store = Arc::new(MemoryStore::new());
    let wakeup = Arc::new(LocalWakeup::new(NonZeroUsize::new(16).unwrap()));
    let bus = Arc::new(LocalEventBus::connected(
        Arc::clone(&wakeup) as Arc<dyn Wakeup>
    ));
    let clock = Arc::new(ManualClock::at(tideloom_core::Timestamp::from_millis(
        1_000,
    )));
    let engine = Engine::new(EngineParts {
        worker: WorkerId("listen-test".to_string()),
        store: Arc::clone(&store) as Arc<dyn InstanceStore>,
        wakeup: Arc::clone(&wakeup) as Arc<dyn Wakeup>,
        definitions: Arc::clone(&definitions) as Arc<dyn tideloom_runtime::DefinitionSource>,
        executors: vec![Arc::new(EmitExecutor::new(
            Arc::clone(&bus) as Arc<dyn EventSink>
        ))],
        events_out: Arc::clone(&bus) as Arc<dyn EventSink>,
        events_in: Arc::clone(&bus) as Arc<dyn EventSource>,
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
    });
    let instance = Instance::new(key, None, json!({"order": "A-9"}), clock.now());
    let id = instance.id.clone();
    store
        .create(instance)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    tideloom_runtime::drive_until_idle(&engine, &clock)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let stored = store
        .get(&id)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(stored.status, Status::Completed);
    assert_eq!(
        stored
            .output
            .as_ref()
            .and_then(|output| output.get("heard")),
        Some(&json!(true))
    );
    let inbox = engine
        .events_in()
        .inbox(&id)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(inbox.is_empty(), "ack follows the release commit");
}

#[tokio::test]
async fn an_emit_wakes_a_listen_one() {
    let wakeup = Arc::new(LocalWakeup::new(std::num::NonZeroUsize::new(16).unwrap()));
    let bus = Arc::new(LocalEventBus::connected(wakeup.clone()));
    let listener = InstanceId::from_string("listener");
    let spec = one("io.example.paid");
    bus.subscribe(&listener, spec.clone())
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    let yaml = r#"
document:
  dsl: '1.0.0'
  namespace: default
  name: paint
  version: '0.1.0'
do:
  - notify:
      emit:
        event:
          with:
            source: https://example.test/orders
            type: io.example.paid
            data:
              order: ${ .order }
"#;
    let definition = Definition::from_yaml(yaml).unwrap_or_else(|error| panic!("{error}"));
    let definitions = Arc::new(MemoryDefinitions::new());
    let key = definitions.publish(definition.clone());
    let executor = EmitExecutor::new(bus.clone());
    executor
        .execute(execution(&definition, key, json!({"order": 7})))
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    let decision = listen::after_claim(&*bus, &listener, &detail(spec))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let ListenStep::Release(release) = decision else {
        panic!("emit should satisfy listen one, got {decision:?}");
    };
    assert_eq!(release.events, vec![json!({"order": 7})]);

    let mut stream = wakeup
        .watch(tideloom_runtime::Offset(0))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let signal = stream
        .next()
        .await
        .unwrap_or_else(|| panic!("emit should hint the listener"))
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(signal.instance, listener);
    assert!(matches!(signal.hint, Hint::Event(_)));
}

#[tokio::test]
async fn listen_one_releases_the_first_matching_event_and_acks_after() {
    let bus = LocalEventBus::new();
    let listener = InstanceId::from_string("listener");
    let spec = one("io.example.paid");
    listen::subscribe(&bus, &listener, spec.clone())
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    let _ = bus
        .publish(event("paid-1", "io.example.paid", json!({"order": 7})))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let _ = bus
        .publish(event("other", "io.example.other", json!({"order": 8})))
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    let decision = listen::after_claim(&bus, &listener, &detail(spec))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let ListenStep::Release(release) = decision else {
        panic!("one matching event should release, got {decision:?}");
    };
    assert_eq!(release.events, vec![json!({"order": 7})]);
    assert_eq!(release.ids, vec!["paid-1".to_string()]);

    // The inbox still holds the event until ack, which follows the commit.
    let pending = bus
        .inbox(&listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(pending.len(), 1);

    listen::ack_after_commit(&bus, &listener, &release.ids)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let left = bus
        .inbox(&listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(left.is_empty(), "acked events must leave the inbox");
}

#[tokio::test]
async fn listen_all_waits_until_every_filter_has_an_event() {
    let bus = LocalEventBus::new();
    let listener = InstanceId::from_string("listener");
    let spec = ListenSpec {
        strategy: ListenStrategy::All,
        filters: vec![
            filter(&[("type", "io.example.paid")]),
            filter(&[("type", "io.example.shipped")]),
        ],
        read: "data".to_string(),
    };
    bus.subscribe(&listener, spec.clone())
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    let _ = bus
        .publish(event("paid", "io.example.paid", json!({"step": "paid"})))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let waiting = listen::after_claim(&bus, &listener, &detail(spec.clone()))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(waiting, ListenStep::Wait);

    let _ = bus
        .publish(event(
            "shipped",
            "io.example.shipped",
            json!({"step": "shipped"}),
        ))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let ListenStep::Release(release) = listen::after_claim(&bus, &listener, &detail(spec))
        .await
        .unwrap_or_else(|error| panic!("{error}"))
    else {
        panic!("both filters matched");
    };
    assert_eq!(
        release.events,
        vec![json!({"step": "paid"}), json!({"step": "shipped"})]
    );
    assert_eq!(release.ids, vec!["paid".to_string(), "shipped".to_string()]);
}

#[tokio::test]
async fn an_event_that_arrives_before_subscribe_is_not_lost() {
    let bus = LocalEventBus::new();
    let listener = InstanceId::from_string("early");
    let _ = bus
        .publish(event(
            "early-paid",
            "io.example.paid",
            json!({"early": true}),
        ))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    bus.subscribe(&listener, one("io.example.paid"))
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    let inbox = bus
        .inbox(&listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].id, "early-paid");
    assert!(listen::satisfied(&one("io.example.paid"), &inbox));
}

#[tokio::test]
async fn an_unmatched_event_does_not_fill_the_inbox() {
    let bus = LocalEventBus::new();
    let listener = InstanceId::from_string("listener");
    bus.subscribe(&listener, one("io.example.paid"))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let _ = bus
        .publish(event("nope", "io.example.other", json!({"no": true})))
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    let inbox = bus
        .inbox(&listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(inbox.is_empty());
    let decision = listen::after_claim(&bus, &listener, &detail(one("io.example.paid")))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(decision, ListenStep::Wait);
}

#[tokio::test]
async fn read_envelope_releases_the_whole_event() {
    let bus = LocalEventBus::new();
    let listener = InstanceId::from_string("listener");
    let spec = ListenSpec {
        strategy: ListenStrategy::One,
        filters: vec![filter(&[
            ("type", "io.example.paid"),
            ("source", "https://example.test/orders"),
        ])],
        read: "envelope".to_string(),
    };
    bus.subscribe(&listener, spec.clone())
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let _ = bus
        .publish(event("paid-1", "io.example.paid", json!({"order": 7})))
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    let ListenStep::Release(release) = listen::after_claim(&bus, &listener, &detail(spec))
        .await
        .unwrap_or_else(|error| panic!("{error}"))
    else {
        panic!("expected a release");
    };
    assert_eq!(release.events[0]["id"], "paid-1");
    assert_eq!(release.events[0]["type"], "io.example.paid");
    assert_eq!(release.events[0]["source"], "https://example.test/orders");
    assert_eq!(release.events[0]["data"]["order"], 7);
}

#[tokio::test]
async fn a_matching_publish_notifies_the_wakeup_bus() {
    let wakeup = Arc::new(LocalWakeup::new(std::num::NonZeroUsize::new(8).unwrap()));
    let bus = LocalEventBus::connected(wakeup.clone());
    let listener = InstanceId::from_string("listener");
    bus.subscribe(&listener, one("io.example.paid"))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let _ = bus
        .publish(event("paid-1", "io.example.paid", json!({"order": 1})))
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    let mut stream = wakeup
        .watch(tideloom_runtime::Offset(0))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let signal = stream
        .next()
        .await
        .unwrap_or_else(|| panic!("a landed event should hint the listener"))
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(signal.instance, listener);
    assert_eq!(signal.hint, Hint::Event("paid-1".to_string()));
}

#[tokio::test]
async fn emit_executor_publishes_the_evaluated_event() {
    let yaml = r#"
document:
  dsl: '1.0.0'
  namespace: default
  name: paint
  version: '0.1.0'
do:
  - notify:
      emit:
        event:
          with:
            source: https://example.test/orders
            type: io.example.paid
            subject: ${ .order }
            data:
              order: ${ .order }
"#;
    let definition = Definition::from_yaml(yaml).unwrap_or_else(|error| panic!("{error}"));
    let definitions = Arc::new(MemoryDefinitions::new());
    let key = definitions.publish(definition.clone());
    let bus = Arc::new(LocalEventBus::new());
    let listener = InstanceId::from_string("listener");
    bus.subscribe(&listener, one("io.example.paid"))
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    let executor = EmitExecutor::new(bus.clone());
    let output = executor
        .execute(execution(&definition, key, json!({"order": "A-9"})))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(output["type"], "io.example.paid");
    assert_eq!(output["source"], "https://example.test/orders");
    assert_eq!(output["subject"], "A-9");
    assert_eq!(output["data"]["order"], "A-9");
    assert!(!executor.idempotent());
    assert_eq!(executor.kind(), TaskKind::Emit);

    let inbox = bus
        .inbox(&listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].data["order"], "A-9");
    assert_eq!(inbox[0].subject.as_deref(), Some("A-9"));
}

#[tokio::test]
async fn any_releases_the_first_of_several_filters() {
    let bus = LocalEventBus::new();
    let listener = InstanceId::from_string("listener");
    let spec = ListenSpec {
        strategy: ListenStrategy::Any,
        filters: vec![
            filter(&[("type", "io.example.paid")]),
            filter(&[("type", "io.example.shipped")]),
        ],
        read: "data".to_string(),
    };
    bus.subscribe(&listener, spec.clone())
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let _ = bus
        .publish(event(
            "shipped",
            "io.example.shipped",
            json!({"which": "ship"}),
        ))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let ListenStep::Release(release) = listen::step(
        &detail(spec),
        &bus.inbox(&listener)
            .await
            .unwrap_or_else(|error| panic!("{error}")),
    ) else {
        panic!("any should release on the first match");
    };
    assert_eq!(release.events, vec![json!({"which": "ship"})]);
}

#[tokio::test]
async fn a_filter_matches_only_when_every_attribute_is_equal() {
    let bus = LocalEventBus::new();
    let listener = InstanceId::from_string("listener");
    let spec = ListenSpec {
        strategy: ListenStrategy::One,
        filters: vec![filter(&[
            ("type", "io.example.paid"),
            ("source", "https://example.test/pay"),
        ])],
        read: "data".to_string(),
    };
    bus.subscribe(&listener, spec)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let _ = bus
        .publish(event("wrong-source", "io.example.paid", json!({"n": 1})))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let inbox = bus
        .inbox(&listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        inbox.is_empty(),
        "a shared type is not enough when source differs"
    );
}

/// `$task.name` and `${ .items | length }` are on the block the interpreter
/// created. The executor publishes that value; it does not evaluate again.
#[tokio::test]
async fn emit_uses_task_name_and_a_length_filter() {
    let yaml = r#"
document:
  dsl: '1.0.0'
  namespace: default
  name: paint
  version: '0.1.0'
do:
  - notify:
      emit:
        event:
          with:
            source: https://example.test/orders
            type: ${ $task.name }
            data:
              count: ${ .items | length }
"#;
    let definition = Definition::from_yaml(yaml).unwrap_or_else(|error| panic!("{error}"));
    let Outcome::Blocked {
        block,
        continuation,
    } = tideloom_core::start(&definition, &json!({"items": [1, 2, 3]}))
    else {
        panic!("emit is a block");
    };
    let tideloom_core::BlockDetail::Emit(with) = block.detail() else {
        panic!("expected an evaluated emit, got {:?}", block.detail());
    };
    assert_eq!(with["type"], "notify");
    assert_eq!(with["data"]["count"], 3);
    let definitions = Arc::new(MemoryDefinitions::new());
    let key = definitions.publish(definition);
    let bus = Arc::new(LocalEventBus::new());
    let executor = EmitExecutor::new(bus);
    let output = executor
        .execute(execution_with(
            key,
            json!({"items": [1, 2, 3]}),
            block.detail().clone(),
            continuation,
        ))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(output["type"], "notify");
    assert_eq!(output["data"]["count"], 3);
}

/// An `emit` inside `for` publishes `$item` from the block. A second
/// evaluation without the loop frame would fault instead of completing.
/// `for` keeps only the last output, so the recording sink keeps both events.
#[tokio::test]
async fn an_emit_inside_for_publishes_each_loop_item() {
    let yaml = r#"
document:
  dsl: '1.0.0'
  namespace: default
  name: paint
  version: '0.1.0'
do:
  - each:
      for:
        each: item
        in: .items
      do:
        - notify:
            emit:
              event:
                with:
                  source: https://example.test/orders
                  type: io.example.item
                  data:
                    item: ${ $item }
            output:
              as: .data.item
"#;
    let inbox = drive_subscribed(yaml, json!({"items": ["pen", "ink"]}), "io.example.item").await;
    let items: Vec<_> = inbox
        .iter()
        .map(|event| event.data["item"].clone())
        .collect();
    assert_eq!(items, vec![json!("pen"), json!("ink")]);
}

/// An `emit` inside `catch` publishes `$error.status` from the block. A
/// re-evaluation without the try frame would fault the instance.
#[tokio::test]
async fn an_emit_inside_catch_publishes_the_error() {
    let yaml = r#"
document:
  dsl: '1.0.0'
  namespace: default
  name: paint
  version: '0.1.0'
do:
  - attempt:
      try:
        - boom:
            raise:
              error:
                type: https://example.test/errors/boom
                status: 503
                title: boom
      catch:
        do:
          - notify:
              emit:
                event:
                  with:
                    source: https://example.test/orders
                    type: io.example.failed
                    data:
                      status: ${ $error.status }
              output:
                as: .data.status
  - after:
      set:
        status: ${ . }
"#;
    let output = drive_emit(yaml, json!({})).await;
    assert_eq!(output["status"], 503, "{output}");
}

/// Subscribe before the run, on an instance the workflow never listens as.
/// Nothing acks that inbox, so both loop events stay there.
async fn drive_subscribed(
    yaml: &str,
    input: serde_json::Value,
    event_type: &str,
) -> Vec<CloudEvent> {
    let definition = Definition::from_yaml(yaml).unwrap_or_else(|error| panic!("{error}"));
    let definitions = Arc::new(MemoryDefinitions::new());
    let key = definitions.publish(definition);
    let store = Arc::new(MemoryStore::new());
    let wakeup = Arc::new(LocalWakeup::new(NonZeroUsize::new(16).unwrap()));
    let bus = Arc::new(LocalEventBus::new());
    let watcher = InstanceId::from_string("watcher");
    bus.subscribe(&watcher, one(event_type))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let clock = Arc::new(ManualClock::at(tideloom_core::Timestamp::from_millis(
        1_000,
    )));
    let engine = Engine::new(EngineParts {
        worker: WorkerId("listen-test".to_string()),
        store: Arc::clone(&store) as Arc<dyn InstanceStore>,
        wakeup: wakeup as Arc<dyn Wakeup>,
        definitions: definitions as Arc<dyn tideloom_runtime::DefinitionSource>,
        executors: vec![Arc::new(EmitExecutor::new(
            Arc::clone(&bus) as Arc<dyn EventSink>
        ))],
        events_out: Arc::clone(&bus) as Arc<dyn EventSink>,
        events_in: Arc::clone(&bus) as Arc<dyn EventSource>,
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
    });
    let instance = Instance::new(key, None, input, clock.now());
    let id = instance.id.clone();
    store
        .create(instance)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    tideloom_runtime::drive_until_idle(&engine, &clock)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let stored = store
        .get(&id)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(stored.status, Status::Completed, "{stored:?}");
    bus.inbox(&watcher)
        .await
        .unwrap_or_else(|error| panic!("{error}"))
}

async fn drive_emit(yaml: &str, input: serde_json::Value) -> serde_json::Value {
    let definition = Definition::from_yaml(yaml).unwrap_or_else(|error| panic!("{error}"));
    let definitions = Arc::new(MemoryDefinitions::new());
    let key = definitions.publish(definition);
    let store = Arc::new(MemoryStore::new());
    let wakeup = Arc::new(LocalWakeup::new(NonZeroUsize::new(16).unwrap()));
    let bus = Arc::new(LocalEventBus::connected(
        Arc::clone(&wakeup) as Arc<dyn Wakeup>
    ));
    let clock = Arc::new(ManualClock::at(tideloom_core::Timestamp::from_millis(
        1_000,
    )));
    let engine = Engine::new(EngineParts {
        worker: WorkerId("listen-test".to_string()),
        store: Arc::clone(&store) as Arc<dyn InstanceStore>,
        wakeup: Arc::clone(&wakeup) as Arc<dyn Wakeup>,
        definitions: Arc::clone(&definitions) as Arc<dyn tideloom_runtime::DefinitionSource>,
        executors: vec![Arc::new(EmitExecutor::new(
            Arc::clone(&bus) as Arc<dyn EventSink>
        ))],
        events_out: Arc::clone(&bus) as Arc<dyn EventSink>,
        events_in: Arc::clone(&bus) as Arc<dyn EventSource>,
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
    });
    let instance = Instance::new(key, None, input, clock.now());
    let id = instance.id.clone();
    store
        .create(instance)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    tideloom_runtime::drive_until_idle(&engine, &clock)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let stored = store
        .get(&id)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(stored.status, Status::Completed, "{stored:?}");
    stored.output.unwrap_or(json!({}))
}

/// A listen parked by engine A becomes claimable when engine B's emit lands,
/// even though A never saw the subscription. Either engine's next pass
/// releases it. The store's `wake`, not a process-local id list, is what
/// makes the listener claimable.
#[tokio::test]
async fn an_emit_on_one_engine_wakes_a_listen_parked_on_another() {
    let definitions = Arc::new(MemoryDefinitions::new());
    let listener = Definition::from_yaml(
        r#"
document:
  dsl: '1.0.0'
  namespace: default
  name: listener
  version: '0.1.0'
do:
  - hear:
      listen:
        to:
          one:
            with:
              type: io.example.paid
  - after:
      set:
        heard: true
"#,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let emitter = Definition::from_yaml(
        r#"
document:
  dsl: '1.0.0'
  namespace: default
  name: emitter
  version: '0.1.0'
do:
  - notify:
      emit:
        event:
          with:
            source: https://example.test/orders
            type: io.example.paid
            data:
              order: ${ .order }
"#,
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let listener_key = definitions.publish(listener);
    let emitter_key = definitions.publish(emitter);
    let store = Arc::new(MemoryStore::new());
    let wakeup = Arc::new(LocalWakeup::new(NonZeroUsize::new(16).unwrap()));
    let bus = Arc::new(LocalEventBus::new());
    let clock = Arc::new(ManualClock::at(tideloom_core::Timestamp::from_millis(
        1_000,
    )));
    let parts = |worker: &str| EngineParts {
        worker: WorkerId(worker.to_string()),
        store: Arc::clone(&store) as Arc<dyn InstanceStore>,
        wakeup: Arc::clone(&wakeup) as Arc<dyn Wakeup>,
        definitions: Arc::clone(&definitions) as Arc<dyn tideloom_runtime::DefinitionSource>,
        executors: vec![Arc::new(EmitExecutor::new(
            Arc::clone(&bus) as Arc<dyn EventSink>
        ))],
        events_out: Arc::clone(&bus) as Arc<dyn EventSink>,
        events_in: Arc::clone(&bus) as Arc<dyn EventSource>,
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
    };
    let engine_a = Engine::new(parts("engine-a"));
    let engine_b = Engine::new(parts("engine-b"));

    let listener = Instance::new(listener_key, None, json!({}), clock.now());
    let listener_id = listener.id.clone();
    store
        .create(listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let parked = engine_a
        .run_once()
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(parked.processed, 1);
    let waiting = store
        .get(&listener_id)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        waiting.status,
        Status::Waiting(tideloom_runtime::WaitReason::Events)
    );
    assert!(
        waiting.wake_at.is_none(),
        "an unsatisfied listen is not claimable"
    );

    let emitter = Instance::new(emitter_key, None, json!({"order": 7}), clock.now());
    let emitter_id = emitter.id.clone();
    store
        .create(emitter)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let emitted = engine_b
        .run_once()
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(emitted.processed >= 1, "{emitted:?}");
    let woken = store
        .get(&listener_id)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        woken.status,
        Status::Runnable,
        "B's emit wakes A's listener through the shared store: {woken:?}"
    );
    assert_eq!(woken.wake_at, Some(clock.now()));

    // A did not publish the event. Its next pass still claims the woken row.
    let released = engine_a
        .run_once()
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(released.processed >= 1, "{released:?}");
    let heard = store
        .get(&listener_id)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(heard.status, Status::Completed, "{heard:?}");
    assert_eq!(
        heard.output.as_ref().and_then(|output| output.get("heard")),
        Some(&json!(true))
    );
    let _ = emitter_id;
}
