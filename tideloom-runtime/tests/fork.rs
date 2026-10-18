//! `fork` branches are child instances. The parent waits, and a child's
//! terminal commit releases it. Cooperative joins in branch order; compete
//! takes the first completion and cancels the rest. A branch fault stops at
//! the join and does not enter a `try` outside the fork.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use serde_json::json;
use tideloom_core::Definition;
use tideloom_core::Timestamp;
use tideloom_runtime::ActivityError;
use tideloom_runtime::ActivityExecutor;
use tideloom_runtime::Clock;
use tideloom_runtime::DefinitionKey;
use tideloom_runtime::Engine;
use tideloom_runtime::EngineParts;
use tideloom_runtime::Hint;
use tideloom_runtime::Instance;
use tideloom_runtime::InstanceId;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::LocalWakeup;
use tideloom_runtime::ManualClock;
use tideloom_runtime::MemoryDefinitions;
use tideloom_runtime::MemoryStore;
use tideloom_runtime::Status;
use tideloom_runtime::TaskExecution;
use tideloom_runtime::TaskKind;
use tideloom_runtime::WaitReason;
use tideloom_runtime::Wakeup;
use tideloom_runtime::WorkerId;

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn definition(body: &str) -> Definition {
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: fork-test\n  version: '0.1.0'\ndo:\n{body}"
    );
    Definition::from_yaml(&yaml).unwrap_or_else(|error| panic!("{error}"))
}

struct Harness {
    engine: Engine,
    store: Arc<MemoryStore>,
    wakeup: Arc<LocalWakeup>,
    clock: Arc<ManualClock>,
    key: DefinitionKey,
}

impl Harness {
    fn new(definition: Definition) -> Self {
        let definitions = MemoryDefinitions::new();
        let key = definitions.publish(definition);
        let store = Arc::new(MemoryStore::new());
        let wakeup = Arc::new(LocalWakeup::new(NonZeroUsize::new(64).expect("64")));
        let clock = Arc::new(ManualClock::at(at(1_000)));
        let engine = Engine::new(EngineParts {
            worker: WorkerId("fork-test".to_string()),
            store: Arc::clone(&store) as Arc<dyn InstanceStore>,
            wakeup: Arc::clone(&wakeup) as Arc<dyn Wakeup>,
            definitions: Arc::new(definitions),
            executors: Vec::new(),
            events_out: Arc::new(Discard),
            events_in: Arc::new(EmptyInbox),
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
        });
        Self {
            engine,
            store,
            wakeup,
            clock,
            key,
        }
    }

    fn with_executor(definition: Definition, executor: Arc<dyn ActivityExecutor>) -> Self {
        let definitions = MemoryDefinitions::new();
        let key = definitions.publish(definition);
        let store = Arc::new(MemoryStore::new());
        let wakeup = Arc::new(LocalWakeup::new(NonZeroUsize::new(64).expect("64")));
        let clock = Arc::new(ManualClock::at(at(1_000)));
        let engine = Engine::new(EngineParts {
            worker: WorkerId("fork-test".to_string()),
            store: Arc::clone(&store) as Arc<dyn InstanceStore>,
            wakeup: Arc::clone(&wakeup) as Arc<dyn Wakeup>,
            definitions: Arc::new(definitions),
            executors: vec![executor],
            events_out: Arc::new(Discard),
            events_in: Arc::new(EmptyInbox),
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
        });
        Self {
            engine,
            store,
            wakeup,
            clock,
            key,
        }
    }

    async fn start(&self) -> InstanceId {
        let instance = Instance::new(self.key.clone(), None, json!({}), self.clock.now());
        let id = instance.id.clone();
        self.store
            .create(instance)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        id
    }

    async fn drive(&self) {
        tideloom_runtime::drive_until_idle(&self.engine, &self.clock)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
    }
}

struct Discard;

#[async_trait]
impl tideloom_runtime::EventSink for Discard {
    async fn publish(
        &self,
        _event: tideloom_runtime::CloudEvent,
    ) -> Result<Vec<InstanceId>, tideloom_runtime::EventError> {
        Ok(Vec::new())
    }
}

struct EmptyInbox;

#[async_trait]
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

#[test]
fn cooperative_three_branches_complete_in_declaration_order() {
    let harness = Harness::new(definition(
        r#"
  - gather:
      fork:
        branches:
          - east:
              set:
                lane: east
          - west:
              set:
                lane: west
          - north:
              set:
                lane: north
  - after:
      set:
        joined: true
"#,
    ));
    block_on(async {
        let id = harness.start().await;
        harness.drive().await;
        let parent = harness.store.get(&id).await.unwrap();
        assert_eq!(parent.status, Status::Completed, "{parent:?}");
        // The join releases the branch outputs in declaration order. The
        // task after the fork sees that array as its input; the workflow
        // output is that task's output, which is how the core walks a list.
        let children = harness.store.children(&id).await.unwrap();
        let joined: Vec<_> = children
            .iter()
            .map(|child| child.output.clone().unwrap_or(Value::Null))
            .collect();
        assert_eq!(
            joined,
            vec![
                json!({"lane": "east"}),
                json!({"lane": "west"}),
                json!({"lane": "north"}),
            ]
        );
        assert_eq!(parent.output, Some(json!({"joined": true})));
        assert_eq!(children.len(), 3);
        assert!(
            children
                .iter()
                .all(|child| child.status == Status::Completed)
        );
        let indexes: Vec<_> = children
            .iter()
            .map(|child| child.parent.as_ref().and_then(|parent| parent.branch))
            .collect();
        assert_eq!(indexes, vec![Some(0), Some(1), Some(2)]);
    });
}

#[test]
fn compete_takes_the_first_completion_and_cancels_the_rest() {
    // The slow branch is an activity with no executor result until the fast
    // branch has already completed. Driving once claims both; the fast one
    // is a `set` and finishes inline, the slow one blocks. The parent's
    // re-evaluation then cancels the slow branch.
    let calls = Arc::new(Mutex::new(0u32));
    let executor = Arc::new(CountingExecutor {
        calls: Arc::clone(&calls),
    });
    let harness = Harness::with_executor(
        definition(
            r#"
  - race:
      fork:
        compete: true
        branches:
          - fast:
              set:
                lane: fast
          - slow:
              call: http
"#,
        ),
        executor,
    );
    block_on(async {
        let id = harness.start().await;
        harness.drive().await;
        let parent = harness.store.get(&id).await.unwrap();
        assert_eq!(parent.status, Status::Completed, "{parent:?}");
        assert_eq!(parent.output, Some(json!({"lane": "fast"})));
        let children = harness.store.children(&id).await.unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].status, Status::Completed);
        assert_eq!(children[1].status, Status::Cancelled);
        // The cancelled branch is claimed and dropped, never executed.
        assert_eq!(*calls.lock().unwrap(), 0);
    });
}

/// An HTTP executor that records calls and never returns a useful body.
struct CountingExecutor {
    calls: Arc<Mutex<u32>>,
}

#[async_trait]
impl ActivityExecutor for CountingExecutor {
    fn kind(&self) -> TaskKind {
        TaskKind::Http
    }

    async fn execute(&self, _task: TaskExecution) -> Result<Value, ActivityError> {
        *self.calls.lock().unwrap() += 1;
        Ok(json!({"called": true}))
    }
}

#[test]
fn a_branch_fault_stops_at_the_join_and_does_not_enter_the_outer_try() {
    let harness = Harness::new(definition(
        r#"
  - guard:
      try:
        - race:
            fork:
              branches:
                - boom:
                    raise:
                      error:
                        type: https://example.test/errors/boom
                        status: 500
                        title: boom
                - fine:
                    set:
                      lane: fine
      catch:
        do:
          - caught:
              set:
                caught: true
"#,
    ));
    block_on(async {
        let id = harness.start().await;
        harness.drive().await;
        let parent = harness.store.get(&id).await.unwrap();
        assert_eq!(parent.status, Status::Faulted, "{parent:?}");
        let fault = parent.fault.clone().unwrap();
        assert_eq!(fault.title(), "boom");
        // The outer try's catch did not run: its export is absent, and the
        // fault is the branch fault rather than a caught-and-reraised one.
        assert!(parent.output.is_none());
        let children = harness.store.children(&id).await.unwrap();
        assert_eq!(children[0].status, Status::Faulted);
        assert_eq!(children[1].status, Status::Cancelled);
        let _ = harness.clock.now();
    });
}

#[test]
fn two_branches_finishing_together_release_the_parent_once() {
    let harness = Harness::new(definition(
        r#"
  - gather:
      fork:
        branches:
          - one:
              set:
                n: 1
          - two:
              set:
                n: 2
"#,
    ));
    block_on(async {
        let id = harness.start().await;
        // First pass creates the children and parks the parent.
        let first = harness.engine.run_once().await.unwrap();
        assert_eq!(first.processed, 1);
        let parent = harness.store.get(&id).await.unwrap();
        assert_eq!(parent.status, Status::Waiting(WaitReason::Join));
        let children = harness.store.children(&id).await.unwrap();
        assert_eq!(children.len(), 2);

        // Both children are claimed and finished in one pass. Each release
        // reads the parent as it is now. The second writes `Runnable` again;
        // `rejoin` re-reads the children, so that second release is idempotent
        // and is not a version conflict.
        let second = harness.engine.run_once().await.unwrap();
        assert_eq!(second.processed, 2);
        assert_eq!(
            second.conflicts, 0,
            "a second release of a Runnable parent is idempotent"
        );
        let parent = harness.store.get(&id).await.unwrap();
        assert_eq!(parent.status, Status::Runnable, "{parent:?}");
        // Spawn bumped the version to 1. Each release reads the current
        // version and writes `Runnable` again, so both land.
        assert_eq!(
            parent.version, 3,
            "spawn, then one release per finished branch"
        );

        // Re-evaluate. A second release must not produce a second output.
        harness.drive().await;
        let parent = harness.store.get(&id).await.unwrap();
        assert_eq!(parent.status, Status::Completed, "{parent:?}");
        assert_eq!(parent.output, Some(json!([{"n": 1}, {"n": 2}])));

        // One ChildDone hint per finished branch. The release itself sends
        // none, so a dropped bus still leaves the parent Runnable.
        let mut seen = 0;
        let mut stream = harness
            .wakeup
            .watch(tideloom_runtime::Offset(0))
            .await
            .unwrap();
        use futures::StreamExt;
        while let Some(signal) = stream.next().await {
            let signal = signal.unwrap();
            assert!(matches!(signal.hint, Hint::ChildDone(_)));
            seen += 1;
            if seen == 2 {
                break;
            }
        }
        assert_eq!(seen, 2);
    });
}

#[test]
fn a_nested_fork_joins_the_inner_branches_before_the_outer_one() {
    let harness = Harness::new(definition(
        r#"
  - outer:
      fork:
        branches:
          - left:
              fork:
                branches:
                  - innerA:
                      set:
                        lane: a
                  - innerB:
                      set:
                        lane: b
          - right:
              set:
                lane: right
"#,
    ));
    block_on(async {
        let id = harness.start().await;
        harness.drive().await;
        let parent = harness.store.get(&id).await.unwrap();
        assert_eq!(parent.status, Status::Completed, "{parent:?}");
        // The outer join releases `[inner join array, right output]`. The
        // workflow has no task after the fork, so that array is the output.
        assert_eq!(
            parent.output,
            Some(json!([[{"lane": "a"}, {"lane": "b"}], {"lane": "right"}]))
        );
        let children = harness.store.children(&id).await.unwrap();
        assert_eq!(children.len(), 2);
        assert!(
            children
                .iter()
                .all(|child| child.status == Status::Completed)
        );
        let inner = harness.store.children(&children[0].id).await.unwrap();
        assert_eq!(inner.len(), 2);
        assert!(inner.iter().all(|child| child.status == Status::Completed));
    });
}

#[test]
fn compete_faults_only_when_every_branch_faults() {
    let harness = Harness::new(definition(
        r#"
  - race:
      fork:
        compete: true
        branches:
          - left:
              raise:
                error:
                  type: https://example.test/errors/left
                  status: 500
                  title: left
          - right:
              raise:
                error:
                  type: https://example.test/errors/right
                  status: 500
                  title: right
"#,
    ));
    block_on(async {
        let id = harness.start().await;
        harness.drive().await;
        let parent = harness.store.get(&id).await.unwrap();
        assert_eq!(parent.status, Status::Faulted, "{parent:?}");
        assert_eq!(parent.fault.unwrap().title(), "left");
        let children = harness.store.children(&id).await.unwrap();
        assert!(children.iter().all(|child| child.status == Status::Faulted));
    });
}

#[test]
fn a_cancelled_branch_is_claimed_and_not_resumed() {
    let calls = Arc::new(Mutex::new(0u32));
    let harness = Harness::with_executor(
        definition(
            r#"
  - race:
      fork:
        compete: true
        branches:
          - fast:
              set:
                lane: fast
          - slow:
              call: http
"#,
        ),
        Arc::new(CountingExecutor {
            calls: Arc::clone(&calls),
        }),
    );
    block_on(async {
        let id = harness.start().await;
        harness.drive().await;
        let children = harness.store.children(&id).await.unwrap();
        let slow = &children[1];
        assert_eq!(slow.status, Status::Cancelled);
        assert!(slow.continuation.is_none());
        // Claiming again must not hand the cancelled branch back.
        let again = harness
            .store
            .claim(
                &WorkerId("other".to_string()),
                harness.clock.now(),
                at(9_000),
                8,
            )
            .await
            .unwrap();
        assert!(again.iter().all(|instance| instance.id != slow.id));
        assert_eq!(*calls.lock().unwrap(), 0);
        let _ = harness.clock.now();
    });
}

/// The early branch is a `set` and finishes in the pass that starts the fork.
/// That release makes the parent claimable again. The late branch is a `wait`
/// and is still running when the parent is claimed in the same later pass,
/// so `rejoin` reparks the parent. The late branch then finishes in that pass
/// and must release the reparked row. A claim-time version snapshot would
/// lose that CAS and leave the parent `Waiting(Join)` with no `wake_at`.
#[test]
fn a_late_branch_releases_a_parent_that_was_reparked_in_the_same_pass() {
    let harness = Harness::new(definition(
        r#"
  - gather:
      fork:
        branches:
          - early:
              set:
                lane: early
          - late:
              do:
                - hold:
                    wait: PT5S
                - mark:
                    set:
                      lane: late
  - after:
      set:
        joined: true
"#,
    ));
    block_on(async {
        let id = harness.start().await;
        // Pass 1: spawn both branches and park the parent on the join.
        let spawned = harness.engine.run_once().await.unwrap();
        assert_eq!(spawned.processed, 1);
        let parent = harness.store.get(&id).await.unwrap();
        assert_eq!(parent.status, Status::Waiting(WaitReason::Join));

        // Pass 2: the early branch finishes and releases the parent. The late
        // branch parks on its wait, which is not due yet.
        let early = harness.engine.run_once().await.unwrap();
        assert!(early.processed >= 2, "{early:?}");
        let parent = harness.store.get(&id).await.unwrap();
        assert_eq!(parent.status, Status::Runnable, "{parent:?}");
        let children = harness.store.children(&id).await.unwrap();
        assert_eq!(children[0].status, Status::Completed);
        assert_eq!(children[1].status, Status::Waiting(WaitReason::Timer));

        // Pass 3: the parent is claimed with the now-due late branch. Rejoin
        // sees the late branch still waiting and reparks; the late branch
        // then completes and must release that new row.
        harness.clock.set(at(1_000 + 5_000));
        let late = harness.engine.run_once().await.unwrap();
        assert!(late.processed >= 2, "{late:?}");
        assert_eq!(
            late.conflicts, 0,
            "the late release must land on the reparked parent"
        );
        let parent = harness.store.get(&id).await.unwrap();
        assert_ne!(
            parent.status,
            Status::Waiting(WaitReason::Join),
            "a reparked parent must not stay waiting after the late branch finishes: {parent:?}"
        );
        assert!(
            parent.wake_at.is_some() || parent.status.is_terminal(),
            "the parent must be claimable or already finished: {parent:?}"
        );

        harness.drive().await;
        let parent = harness.store.get(&id).await.unwrap();
        assert_eq!(parent.status, Status::Completed, "{parent:?}");
        assert_eq!(parent.output, Some(json!({"joined": true})));
    });
}

/// A branch's first activity runs in the pass that starts the branch, not in
/// a later claim that only existed to unpark it.
#[test]
fn a_branch_runs_its_first_activity_in_the_pass_that_starts_it() {
    let calls = Arc::new(Mutex::new(0u32));
    let harness = Harness::with_executor(
        definition(
            r#"
  - gather:
      fork:
        branches:
          - work:
              call: http
"#,
        ),
        Arc::new(CountingExecutor {
            calls: Arc::clone(&calls),
        }),
    );
    block_on(async {
        let id = harness.start().await;
        // The parent reaches the fork and spawns the branch.
        let spawned = harness.engine.run_once().await.unwrap();
        assert_eq!(spawned.processed, 1);
        assert_eq!(
            *calls.lock().unwrap(),
            0,
            "the parent step does not run the branch"
        );
        let children = harness.store.children(&id).await.unwrap();
        assert_eq!(children.len(), 1);
        assert!(
            children[0].continuation.is_none(),
            "the branch has not started"
        );

        // The next pass claims the fresh branch and runs its activity.
        let started = harness.engine.run_once().await.unwrap();
        assert_eq!(started.processed, 1, "{started:?}");
        assert_eq!(
            *calls.lock().unwrap(),
            1,
            "the first activity runs in the pass that starts the branch"
        );
        let child = harness.store.get(&children[0].id).await.unwrap();
        assert_eq!(child.status, Status::Completed, "{child:?}");
    });
}

/// A branch stores the fork input as its own input. The task it starts at is
/// `root_task`, not a key hidden in that value.
#[test]
fn a_branch_keeps_the_parent_input_and_records_its_position() {
    let harness = Harness::new(definition(
        r#"
  - gather:
      fork:
        branches:
          - left:
              set:
                lane: left
          - right:
              set:
                lane: right
"#,
    ));
    block_on(async {
        let input = json!({"order": "A-9", "nested": {"n": 1}});
        let instance = Instance::new(
            harness.key.clone(),
            None,
            input.clone(),
            harness.clock.now(),
        );
        let id = instance.id.clone();
        harness.store.create(instance).await.unwrap();

        let spawned = harness.engine.run_once().await.unwrap();
        assert_eq!(spawned.processed, 1);
        let parent = harness.store.get(&id).await.unwrap();
        assert!(parent.root_task.is_none(), "{parent:?}");
        assert_eq!(parent.input, input);

        let children = harness.store.children(&id).await.unwrap();
        assert_eq!(children.len(), 2);
        let positions: Vec<_> = children
            .iter()
            .map(|child| child.root_task.clone())
            .collect();
        assert!(
            positions.iter().all(|position| position.is_some()),
            "{positions:?}"
        );
        assert_ne!(positions[0], positions[1]);
        for child in &children {
            assert_eq!(
                child.input, input,
                "a branch input is the parent input, byte for byte"
            );
            assert!(
                !child.input.to_string().contains("tideloom.branch"),
                "the position must not ride inside the input: {}",
                child.input
            );
        }
    });
}
