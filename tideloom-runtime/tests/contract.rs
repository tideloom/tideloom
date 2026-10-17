//! The runtime contracts as later milestone MRs rely on them: the traits are
//! object-safe, the in-memory helpers behave, and task routing covers every
//! node kind.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tideloom_core::Definition;
use tideloom_core::Timestamp;
use tideloom_runtime::ActivityExecutor;
use tideloom_runtime::Clock;
use tideloom_runtime::Commit;
use tideloom_runtime::DefinitionSource;
use tideloom_runtime::EventSink;
use tideloom_runtime::EventSource;
use tideloom_runtime::Instance;
use tideloom_runtime::InstanceId;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::ManualClock;
use tideloom_runtime::MemoryDefinitions;
use tideloom_runtime::Status;
use tideloom_runtime::TaskKind;
use tideloom_runtime::WaitReason;
use tideloom_runtime::Wakeup;

fn sample(name: &str) -> Definition {
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: {name}\n  version: '0.1.0'\ndo:\n  - paint:\n      set:\n        color: red\n"
    );
    Definition::from_yaml(&yaml).unwrap_or_else(|error| panic!("{error}"))
}

#[allow(dead_code)]
fn traits_are_object_safe(
    _store: Arc<dyn InstanceStore>,
    _wakeup: Arc<dyn Wakeup>,
    _executor: Arc<dyn ActivityExecutor>,
    _definitions: Arc<dyn DefinitionSource>,
    _sink: Arc<dyn EventSink>,
    _source: Arc<dyn EventSource>,
    _clock: Arc<dyn Clock>,
) {
}

#[test]
fn instance_ids_are_unique_and_sort_by_creation() {
    let first = InstanceId::new();
    let second = InstanceId::new();
    assert_ne!(first, second);
    assert!(first <= second, "{first} should not sort after {second}");
    assert_eq!(InstanceId::from_string(first.as_str()), first);
}

#[test]
fn a_new_instance_is_runnable_now() {
    let definitions = MemoryDefinitions::new();
    let key = definitions.publish(sample("paint"));
    let created = Timestamp::from_millis(10);
    let instance = Instance::new(key.clone(), None, json!({"n": 1}), created);
    assert_eq!(instance.status, Status::Runnable);
    assert_eq!(instance.wake_at, Some(created));
    assert_eq!(instance.version, 0);
    assert!(instance.continuation.is_none());
    assert_eq!(instance.definition, key);
    assert!(!Status::Waiting(WaitReason::Join).is_terminal());
    assert!(Status::Cancelled.is_terminal());

    let commit = Commit::new(instance.id.clone(), 0, Status::Completed, created);
    assert!(commit.release_lease);
    assert!(commit.step_results.is_empty());
    assert!(commit.in_flight.is_none());
    assert!(instance.root_task.is_none());
    assert!(instance.in_flight.is_none());
    assert!(instance.lease.is_none());
}

#[test]
fn memory_definitions_resolve_by_key_and_by_identity() {
    let definitions = MemoryDefinitions::new();
    let key = definitions.publish(sample("paint"));
    assert_eq!(key.namespace, "default");
    assert_eq!(key.name, "paint");
    assert_eq!(key.version, "0.1.0");
    assert_eq!(key.content_hash.len(), 64);

    let runtime = tokio_lite::block_on(async {
        let pinned = definitions
            .get(&key)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(pinned.identity().name(), "paint");
        let (latest_key, latest) = definitions
            .latest("default", "paint", "0.1.0")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(latest_key, key);
        assert_eq!(latest.content_hash(), pinned.content_hash());
        definitions
            .latest("default", "missing", "0.1.0")
            .await
            .is_err()
    });
    assert!(runtime);
}

#[test]
fn manual_clock_only_moves_when_told() {
    let clock = ManualClock::at(Timestamp::from_millis(100));
    assert_eq!(clock.now(), Timestamp::from_millis(100));
    clock.advance(Duration::from_millis(5));
    assert_eq!(clock.now(), Timestamp::from_millis(105));
    clock.set(Timestamp::from_millis(1));
    assert_eq!(clock.now(), Timestamp::from_millis(1));
}

#[test]
fn every_effectful_node_kind_routes_to_an_executor() {
    let yaml = r#"
document:
  dsl: '1.0.3'
  namespace: default
  name: kinds
  version: '0.1.0'
do:
  - a: { call: http }
  - b: { call: grpc }
  - c: { call: openapi }
  - d: { call: asyncapi }
  - e: { call: a2a }
  - f: { call: mcp }
  - g: { emit: {} }
  - h: { run: { shell: { command: ls } } }
  - i: { run: { script: { language: js, code: "1" } } }
  - j: { run: { container: { image: alpine } } }
  - k: { run: { workflow: { namespace: n, name: w, version: '1' } } }
  - l: { wait: {} }
  - m: { set: { x: 1 } }
"#;
    let definition = Definition::from_yaml(yaml).unwrap_or_else(|error| panic!("{error}"));
    let kinds: Vec<Option<TaskKind>> = definition
        .root()
        .children()
        .iter()
        .map(|node| TaskKind::of(node.kind()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            Some(TaskKind::Http),
            Some(TaskKind::Grpc),
            Some(TaskKind::OpenApi),
            Some(TaskKind::AsyncApi),
            Some(TaskKind::A2a),
            Some(TaskKind::Mcp),
            Some(TaskKind::Emit),
            Some(TaskKind::Shell),
            Some(TaskKind::Script),
            Some(TaskKind::Container),
            None,
            None,
            None,
        ]
    );
    for node in definition.root().children() {
        assert_eq!(
            TaskKind::of(node.kind()).is_some(),
            node.effectful()
                && !matches!(
                    node.kind(),
                    tideloom_core::NodeKind::Run(tideloom_core::RunKind::Workflow)
                ),
            "{}",
            node.position()
        );
    }
}

/// A tiny executor so the async traits can be polled without tokio.
mod tokio_lite {
    use std::future::Future;
    use std::pin::pin;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;

    pub fn block_on<F: Future>(future: F) -> F::Output {
        let mut context = Context::from_waker(Waker::noop());
        let mut future = pin!(future);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }
}
