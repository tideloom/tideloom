//! `run: workflow` starts a child instance. The parent waits on
//! `Waiting(Child)` and is released when the child reaches a terminal status,
//! through the same re-evaluation a fork join uses. `await: false` continues
//! the parent with the child id. A child fault is an ordinary effectful fault,
//! so a parent `try` can catch it. An unpublished target faults the parent.

use std::num::NonZeroUsize;
use std::sync::Arc;

use serde_json::Value;
use serde_json::json;
use tideloom_core::Definition;
use tideloom_core::Timestamp;
use tideloom_runtime::Clock;
use tideloom_runtime::DefinitionKey;
use tideloom_runtime::Engine;
use tideloom_runtime::EngineParts;
use tideloom_runtime::Instance;
use tideloom_runtime::InstanceId;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::LocalWakeup;
use tideloom_runtime::ManualClock;
use tideloom_runtime::MemoryDefinitions;
use tideloom_runtime::MemoryStore;
use tideloom_runtime::Status;
use tideloom_runtime::WaitReason;
use tideloom_runtime::Wakeup;
use tideloom_runtime::WorkerId;

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn document(namespace: &str, name: &str, version: &str, tasks: &str) -> Definition {
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: {namespace}\n  name: {name}\n  version: '{version}'\ndo:\n{tasks}"
    );
    Definition::from_yaml(&yaml).unwrap_or_else(|error| panic!("{error}"))
}

struct Harness {
    engine: Engine,
    store: Arc<MemoryStore>,
    clock: ManualClock,
    parent: DefinitionKey,
}

impl Harness {
    fn new(parent: Definition, children: &[Definition]) -> Self {
        let definitions = MemoryDefinitions::new();
        let parent = definitions.publish(parent);
        for child in children {
            definitions.publish(child.clone());
        }
        let store = Arc::new(MemoryStore::new());
        let wakeup = Arc::new(LocalWakeup::new(NonZeroUsize::new(64).expect("64")));
        let clock = ManualClock::at(at(1_000));
        let engine = Engine::new(EngineParts {
            worker: WorkerId("child-test".to_string()),
            store: Arc::clone(&store) as Arc<dyn InstanceStore>,
            wakeup: wakeup as Arc<dyn Wakeup>,
            definitions: Arc::new(definitions),
            executors: Vec::new(),
            events_out: Arc::new(Discard),
            events_in: Arc::new(EmptyInbox),
            clock: Arc::new(ManualClock::at(at(1_000))),
        });
        Self {
            engine,
            store,
            clock,
            parent,
        }
    }

    async fn start(&self, input: Value) -> InstanceId {
        let instance = Instance::new(self.parent.clone(), None, input, self.clock.now());
        let id = instance.id.clone();
        self.store
            .create(instance)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        id
    }

    async fn drive(&self) {
        self.engine
            .drive_until_idle(&self.clock)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
    }
}

struct Discard;

#[async_trait::async_trait]
impl tideloom_runtime::EventSink for Discard {
    async fn publish(
        &self,
        _event: tideloom_runtime::CloudEvent,
    ) -> Result<(), tideloom_runtime::EventError> {
        Ok(())
    }
}

struct EmptyInbox;

#[async_trait::async_trait]
impl tideloom_runtime::EventSource for EmptyInbox {
    async fn subscribe(
        &self,
        _instance: &InstanceId,
        _spec: tideloom_core::ListenSpec,
    ) -> Result<(), tideloom_runtime::EventError> {
        Ok(())
    }

    async fn unsubscribe(
        &self,
        _instance: &InstanceId,
    ) -> Result<(), tideloom_runtime::EventError> {
        Ok(())
    }

    async fn inbox(
        &self,
        _instance: &InstanceId,
    ) -> Result<Vec<tideloom_runtime::CloudEvent>, tideloom_runtime::EventError> {
        Ok(Vec::new())
    }

    async fn ack(
        &self,
        _instance: &InstanceId,
        _ids: &[String],
    ) -> Result<(), tideloom_runtime::EventError> {
        Ok(())
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::pin::pin;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;

    let mut context = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
}

fn receipt() -> Definition {
    document(
        "orders",
        "receipt",
        "1.0.0",
        r#"
  - stamp:
      set:
        receipt: ${ .orderId }
"#,
    )
}

#[test]
fn an_awaited_child_completes_and_the_parent_continues_with_its_output() {
    let harness = Harness::new(
        document(
            "orders",
            "checkout",
            "1.0.0",
            r#"
  - take:
      run:
        workflow:
          namespace: orders
          name: receipt
          version: '1.0.0'
          input:
            orderId: ${ .id }
  - seal:
      set:
        sealed: ${ .receipt }
"#,
        ),
        &[receipt()],
    );
    let id = block_on(harness.start(json!({"id": 7})));
    block_on(harness.drive());

    let parent = block_on(harness.store.get(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(parent.status, Status::Completed);
    // The child output replaces the parent input. `set` merges onto that.
    assert_eq!(
        parent.output,
        Some(json!({"orderId": 7, "receipt": 7, "sealed": 7}))
    );
    assert!(parent.continuation.is_none());

    let children = block_on(harness.store.children(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(children.len(), 1);
    let child = &children[0];
    assert_eq!(child.status, Status::Completed);
    // The child keeps the workflow input it was given. The parent input's
    // `id` is not part of that object; `receipt` is computed from `orderId`.
    assert_eq!(child.output, Some(json!({"orderId": 7, "receipt": 7})));
    assert_eq!(child.input, json!({"orderId": 7}));
    assert!(
        child
            .parent
            .as_ref()
            .is_some_and(|parent| parent.branch.is_none())
    );
    // The child is pinned to the definition published at creation, not to a
    // name the parent continuation would have to remember.
    assert_eq!(child.definition.name, "receipt");
    assert_eq!(child.definition.namespace, "orders");
    assert_eq!(child.definition.version, "1.0.0");
    assert!(!child.definition.content_hash.is_empty());
}

#[test]
fn await_false_releases_the_parent_with_the_child_id_and_still_runs_the_child() {
    let harness = Harness::new(
        document(
            "orders",
            "checkout",
            "1.0.0",
            r#"
  - take:
      run:
        workflow:
          namespace: orders
          name: receipt
          version: '1.0.0'
          input:
            orderId: ${ .id }
        await: false
  - note:
      set:
        started: ${ . }
"#,
        ),
        &[receipt()],
    );
    let id = block_on(harness.start(json!({"id": 9})));
    block_on(harness.drive());

    let parent = block_on(harness.store.get(&id)).unwrap_or_else(|error| panic!("{error}"));
    let children = block_on(harness.store.children(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(children.len(), 1);
    let child = &children[0];
    assert_eq!(parent.status, Status::Completed, "{parent:?}");
    assert_eq!(children.len(), 1, "{parent:?}");
    assert_eq!(parent.output, Some(json!({"started": child.id.as_str()})));
    assert_eq!(child.status, Status::Completed);
    assert_eq!(child.output, Some(json!({"orderId": 9, "receipt": 9})));
    assert!(
        child
            .parent
            .as_ref()
            .is_some_and(|parent| parent.branch.is_none())
    );
}

#[test]
fn a_child_fault_is_caught_by_the_parent_try() {
    let child = document(
        "orders",
        "receipt",
        "1.0.0",
        r#"
  - boom:
      raise:
        error:
          type: https://example.test/down
          status: 503
          title: down
"#,
    );
    let harness = Harness::new(
        document(
            "orders",
            "checkout",
            "1.0.0",
            r#"
  - take:
      try:
        - child:
            run:
              workflow:
                namespace: orders
                name: receipt
                version: '1.0.0'
      catch:
        as: error
        do:
          - recover:
              set:
                caught: ${ $error.status }
"#,
        ),
        &[child],
    );
    let id = block_on(harness.start(json!({})));
    block_on(harness.drive());

    let parent = block_on(harness.store.get(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(parent.status, Status::Completed, "{parent:?}");
    assert_eq!(parent.output, Some(json!({"caught": 503})));

    let children = block_on(harness.store.children(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].status, Status::Faulted);
    let fault = children[0].fault.as_ref().expect("child fault");
    assert_eq!(fault.status(), 503);
}

#[test]
fn an_uncaught_child_fault_faults_the_parent() {
    let child = document(
        "orders",
        "receipt",
        "1.0.0",
        r#"
  - boom:
      raise:
        error:
          type: https://example.test/down
          status: 503
          title: down
"#,
    );
    let harness = Harness::new(
        document(
            "orders",
            "checkout",
            "1.0.0",
            r#"
  - take:
      run:
        workflow:
          namespace: orders
          name: receipt
          version: '1.0.0'
"#,
        ),
        &[child],
    );
    let id = block_on(harness.start(json!({})));
    block_on(harness.drive());

    let parent = block_on(harness.store.get(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(parent.status, Status::Faulted);
    let fault = parent.fault.as_ref().expect("parent fault");
    assert_eq!(fault.status(), 503);
    assert_eq!(fault.title(), "down");
}

#[test]
fn a_workflow_that_calls_itself_runs_three_deep_and_returns() {
    let recursive = document(
        "orders",
        "descend",
        "1.0.0",
        r#"
  - step:
      set:
        depth: ${ .depth + 1 }
  - again:
      if: .depth < 3
      run:
        workflow:
          namespace: orders
          name: descend
          version: '1.0.0'
          input:
            depth: ${ .depth }
"#,
    );
    let harness = Harness::new(recursive.clone(), &[recursive]);
    let id = block_on(harness.start(json!({"depth": 0})));
    block_on(harness.drive());

    let parent = block_on(harness.store.get(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(parent.status, Status::Completed, "{parent:?}");
    // Each level's output is the child's output, which is the leaf's.
    assert_eq!(parent.output, Some(json!({"depth": 3})));

    let mut current = id;
    let mut depths = Vec::new();
    for _ in 0..2 {
        let children =
            block_on(harness.store.children(&current)).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(children.len(), 1, "each non-leaf level spawns one child");
        let child = &children[0];
        assert_eq!(child.status, Status::Completed);
        assert!(
            child
                .parent
                .as_ref()
                .is_some_and(|parent| parent.branch.is_none())
        );
        depths.push(child.output.clone());
        current = child.id.clone();
    }
    assert_eq!(
        depths,
        vec![Some(json!({"depth": 3})), Some(json!({"depth": 3})),]
    );
    let leaves =
        block_on(harness.store.children(&current)).unwrap_or_else(|error| panic!("{error}"));
    assert!(leaves.is_empty(), "the third level does not spawn");
}

#[test]
fn an_unpublished_child_workflow_faults_the_parent() {
    let harness = Harness::new(
        document(
            "orders",
            "checkout",
            "1.0.0",
            r#"
  - take:
      run:
        workflow:
          namespace: orders
          name: missing
          version: '9.9.9'
"#,
        ),
        &[],
    );
    let id = block_on(harness.start(json!({})));
    block_on(harness.drive());

    let parent = block_on(harness.store.get(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(parent.status, Status::Faulted);
    let fault = parent.fault.as_ref().expect("configuration fault");
    assert_eq!(fault.status(), 400);
    assert!(
        fault.detail().unwrap_or("").contains("not published"),
        "{fault:?}"
    );
    let children = block_on(harness.store.children(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert!(children.is_empty(), "an unpublished target spawns nothing");
}

#[test]
fn a_parent_waits_on_child_until_the_child_is_terminal() {
    let harness = Harness::new(
        document(
            "orders",
            "checkout",
            "1.0.0",
            r#"
  - take:
      run:
        workflow:
          namespace: orders
          name: receipt
          version: '1.0.0'
"#,
        ),
        &[receipt()],
    );
    let id = block_on(harness.start(json!({"orderId": 1})));
    let stepped = block_on(harness.engine.run_once(harness.clock.now()))
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(stepped.processed >= 1);

    let parent = block_on(harness.store.get(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(parent.status, Status::Waiting(WaitReason::Child));
    assert!(parent.continuation.is_some());
    let children = block_on(harness.store.children(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(children.len(), 1);
    assert!(!children[0].definition.content_hash.is_empty());
}

/// A child that pauses on `wait` must not release the parent early. The
/// existing timer path claims the child when `wake_at` is due; the parent
/// stays `Waiting(Child)` until that commit is terminal.
#[test]
fn a_child_wait_holds_the_parent_until_the_timer_is_due() {
    let child = document(
        "orders",
        "receipt",
        "1.0.0",
        r#"
  - pause:
      wait: PT1S
  - stamp:
      set:
        receipt: ready
"#,
    );
    let harness = Harness::new(
        document(
            "orders",
            "checkout",
            "1.0.0",
            r#"
  - take:
      run:
        workflow:
          namespace: orders
          name: receipt
          version: '1.0.0'
"#,
        ),
        &[child],
    );
    let id = block_on(harness.start(json!({})));
    block_on(harness.engine.run_once(harness.clock.now()))
        .unwrap_or_else(|error| panic!("{error}"));
    // The child is spawned and claimed in later passes. Drive once more so
    // the child reaches its wait, then stop before the clock advances.
    block_on(harness.engine.run_once(harness.clock.now()))
        .unwrap_or_else(|error| panic!("{error}"));
    let parent = block_on(harness.store.get(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        parent.status,
        Status::Waiting(WaitReason::Child),
        "{parent:?}"
    );
    let children = block_on(harness.store.children(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].status, Status::Waiting(WaitReason::Timer));
    assert!(
        children[0]
            .wake_at
            .is_some_and(|at| at > harness.clock.now())
    );

    block_on(harness.drive());
    let parent = block_on(harness.store.get(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(parent.status, Status::Completed, "{parent:?}");
    assert_eq!(parent.output, Some(json!({"receipt": "ready"})));
}

/// A fork branch that itself runs a workflow must not be counted as a join
/// sibling. The branch index is `Some`; the workflow child's is `None`.
#[test]
fn a_workflow_child_inside_a_fork_branch_does_not_change_the_join() {
    let harness = Harness::new(
        document(
            "orders",
            "checkout",
            "1.0.0",
            r#"
  - gather:
      fork:
        branches:
          - east:
              run:
                workflow:
                  namespace: orders
                  name: receipt
                  version: '1.0.0'
                  input:
                    orderId: 1
          - west:
              set:
                lane: west
"#,
        ),
        &[receipt()],
    );
    let id = block_on(harness.start(json!({})));
    block_on(harness.drive());
    let parent = block_on(harness.store.get(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(parent.status, Status::Completed, "{parent:?}");
    assert_eq!(
        parent.output,
        Some(json!([{"orderId": 1, "receipt": 1}, {"lane": "west"}]))
    );
    let branches = block_on(harness.store.children(&id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(branches.len(), 2);
    assert!(branches.iter().all(|branch| {
        branch
            .parent
            .as_ref()
            .is_some_and(|parent| parent.branch.is_some())
    }));
    let east =
        block_on(harness.store.children(&branches[0].id)).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(east.len(), 1);
    assert!(
        east[0]
            .parent
            .as_ref()
            .is_some_and(|parent| parent.branch.is_none())
    );
    assert_eq!(east[0].status, Status::Completed);
}
