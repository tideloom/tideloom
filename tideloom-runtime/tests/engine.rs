//! The claim loop: a timer, a retry, a workflow timeout, a lost signal, and
//! two workers that must not run the same step.
//!
//! Signals are hints. These tests never read the bus to decide what ran. A
//! dropped notify still finishes once the sweeper returns the lease and the
//! clock reaches `wake_at`.

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use async_trait::async_trait;
use serde_json::Value;
use serde_json::json;
use tideloom_core::Definition;
use tideloom_core::ListenSpec;
use tideloom_core::Timestamp;
use tideloom_runtime::ActivityError;
use tideloom_runtime::ActivityExecutor;
use tideloom_runtime::Clock;
use tideloom_runtime::CloudEvent;
use tideloom_runtime::DefinitionKey;
use tideloom_runtime::Engine;
use tideloom_runtime::EngineParts;
use tideloom_runtime::EventError;
use tideloom_runtime::EventSink;
use tideloom_runtime::EventSource;
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

fn run<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    match pin!(future).as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("engine step unexpectedly suspended"),
    }
}

fn definition(head: &str, tasks: &str) -> Definition {
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: engine\n  version: '0.1.0'\n{head}do:\n{tasks}"
    );
    Definition::from_yaml(&yaml).unwrap_or_else(|error| panic!("{error}"))
}

struct Harness {
    engine: Engine,
    other: Engine,
    store: Arc<MemoryStore>,
    wakeup: Arc<DroppingWakeup>,
    clock: ManualClock,
    key: DefinitionKey,
}

impl Harness {
    fn new(head: &str, tasks: &str) -> Self {
        let definitions = Arc::new(MemoryDefinitions::new());
        let key = definitions.publish(definition(head, tasks));
        let store = Arc::new(MemoryStore::new());
        let wakeup = Arc::new(DroppingWakeup::new());
        let clock = ManualClock::at(at(1_000));
        let calls = Arc::new(AtomicUsize::new(0));
        // Registered, so the retry test replaces `http_call`. A workflow that
        // does not register one still uses the core client.
        let executors: Vec<Arc<dyn ActivityExecutor>> =
            vec![Arc::new(CountingHttp::new(Arc::clone(&calls)))];
        let engine = engine_for(
            "engine-a",
            &store,
            &wakeup,
            &definitions,
            executors.clone(),
            &clock,
        );
        let other = engine_for("engine-b", &store, &wakeup, &definitions, executors, &clock);
        Self {
            engine,
            other,
            store,
            wakeup,
            clock,
            key,
        }
    }

    fn start(&self, input: Value) -> InstanceId {
        let instance = Instance::new(self.key.clone(), None, input, self.clock.now());
        let id = instance.id.clone();
        run(self.store.create(instance)).unwrap();
        id
    }

    fn drive(&self) -> usize {
        run(self.engine.drive_until_idle(&self.clock)).unwrap()
    }
}

/// Poll a future that may wait on a tokio mutex, as the journal tests do not.
fn block_on<F: Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(future)
}

fn engine_for(
    worker: &str,
    store: &Arc<MemoryStore>,
    wakeup: &Arc<DroppingWakeup>,
    definitions: &Arc<MemoryDefinitions>,
    executors: Vec<Arc<dyn ActivityExecutor>>,
    clock: &ManualClock,
) -> Engine {
    Engine::new(EngineParts {
        worker: WorkerId(worker.to_string()),
        store: Arc::clone(store) as Arc<dyn InstanceStore>,
        wakeup: Arc::clone(wakeup) as Arc<dyn Wakeup>,
        definitions: Arc::clone(definitions) as Arc<dyn tideloom_runtime::DefinitionSource>,
        executors,
        events_out: Arc::new(Silent),
        events_in: Arc::new(Silent),
        clock: Arc::new(ManualClock::at(clock.now())),
    })
}

/// A bus that records notifies and can drop every one of them.
///
/// `run_once` never reads this. Dropping the log is how the sweeper test
/// proves a lost signal is only a hint.
struct DroppingWakeup {
    inner: LocalWakeup,
    dropped: Mutex<bool>,
    notifies: AtomicUsize,
}

impl DroppingWakeup {
    fn new() -> Self {
        Self {
            inner: LocalWakeup::new(NonZeroUsize::new(32).expect("32")),
            dropped: Mutex::new(false),
            notifies: AtomicUsize::new(0),
        }
    }

    fn drop_all(&self) {
        *self.dropped.lock().expect("drop flag") = true;
    }

    fn notifies(&self) -> usize {
        self.notifies.load(Ordering::SeqCst)
    }

    fn dropped(&self) -> bool {
        *self.dropped.lock().expect("drop flag")
    }
}

#[async_trait]
impl Wakeup for DroppingWakeup {
    async fn notify(
        &self,
        instance: InstanceId,
        hint: Hint,
    ) -> Result<tideloom_runtime::Offset, tideloom_runtime::BusError> {
        self.notifies.fetch_add(1, Ordering::SeqCst);
        if *self.dropped.lock().expect("drop flag") {
            return Ok(tideloom_runtime::Offset(0));
        }
        self.inner.notify(instance, hint).await
    }

    async fn watch(
        &self,
        from: tideloom_runtime::Offset,
    ) -> Result<tideloom_runtime::SignalStream, tideloom_runtime::BusError> {
        self.inner.watch(from).await
    }

    async fn commit_offset(
        &self,
        offset: tideloom_runtime::Offset,
    ) -> Result<(), tideloom_runtime::BusError> {
        self.inner.commit_offset(offset).await
    }

    async fn committed(&self) -> Result<tideloom_runtime::Offset, tideloom_runtime::BusError> {
        self.inner.committed().await
    }
}

struct Silent;

#[async_trait]
impl EventSink for Silent {
    async fn publish(&self, _: CloudEvent) -> Result<(), EventError> {
        Ok(())
    }
}

#[async_trait]
impl EventSource for Silent {
    async fn subscribe(&self, _: &InstanceId, _: ListenSpec) -> Result<(), EventError> {
        Ok(())
    }

    async fn unsubscribe(&self, _: &InstanceId) -> Result<(), EventError> {
        Ok(())
    }

    async fn inbox(&self, _: &InstanceId) -> Result<Vec<CloudEvent>, EventError> {
        Ok(Vec::new())
    }

    async fn ack(&self, _: &InstanceId, _: &[String]) -> Result<(), EventError> {
        Ok(())
    }
}

struct CountingHttp {
    hits: Arc<AtomicUsize>,
    fail_first: bool,
}

impl CountingHttp {
    fn new(hits: Arc<AtomicUsize>) -> Self {
        Self {
            hits,
            fail_first: true,
        }
    }
}

#[async_trait]
impl ActivityExecutor for CountingHttp {
    fn kind(&self) -> TaskKind {
        TaskKind::Http
    }

    fn idempotent(&self) -> bool {
        true
    }

    async fn execute(&self, task: TaskExecution) -> Result<Value, ActivityError> {
        let hit = self.hits.fetch_add(1, Ordering::SeqCst);
        if self.fail_first && hit == 0 {
            return Err(ActivityError::Fault(tideloom_core::Fault::communication(
                task.key.position(),
                503,
                "down",
            )));
        }
        Ok(json!({"paid": true}))
    }
}

#[test]
fn a_wait_completes_when_the_clock_reaches_wake_at() {
    let harness = Harness::new(
        "",
        r#"
  - pause:
      wait:
        seconds: 30
  - after:
      set:
        done: true
"#,
    );
    let id = harness.start(json!({}));
    let parked = run(harness.engine.run_once(harness.clock.now())).unwrap();
    assert_eq!(parked.processed, 1);
    let waiting = run(harness.store.get(&id)).unwrap();
    assert_eq!(waiting.status, Status::Waiting(WaitReason::Timer));
    assert_eq!(waiting.wake_at, Some(at(31_000)));

    // Still in the future: claim finds nothing.
    let early = run(harness.engine.run_once(at(30_999))).unwrap();
    assert_eq!(early.processed, 0);
    assert_eq!(
        run(harness.store.get(&id)).unwrap().status,
        Status::Waiting(WaitReason::Timer)
    );

    harness.clock.set(at(31_000));
    let stepped = harness.drive();
    assert!(stepped >= 1);
    let done = run(harness.store.get(&id)).unwrap();
    assert_eq!(done.status, Status::Completed);
    assert_eq!(
        done.output.as_ref().and_then(|output| output.get("done")),
        Some(&json!(true))
    );
    assert_eq!(harness.clock.now(), at(31_000));
}

#[test]
fn a_retry_is_claimed_again_at_wake_at() {
    let harness = Harness::new(
        "",
        r#"
  - charge:
      try:
        - callPay:
            call: http
            with:
              method: post
              endpoint: http://127.0.0.1:9/pay
      catch:
        retry:
          delay: PT3S
          backoff:
            constant: {}
          limit:
            attempt:
              count: 1
"#,
    );
    let id = harness.start(json!({}));
    let first = run(harness.engine.run_once(harness.clock.now())).unwrap();
    assert_eq!(first.processed, 1);
    let waiting = run(harness.store.get(&id)).unwrap();
    assert_eq!(waiting.status, Status::Waiting(WaitReason::Timer));
    assert_eq!(waiting.wake_at, Some(at(4_000)), "{waiting:?}");
    let steps = run(harness.store.steps(&id)).unwrap();
    assert!(
        steps
            .iter()
            .any(|step| matches!(step.outcome, tideloom_runtime::StepOutcome::Fault(_))),
        "the failed call is journalled before the backoff, {steps:?}"
    );

    harness.clock.set(at(4_000));
    let stepped = harness.drive();
    assert!(stepped >= 1);
    let done = run(harness.store.get(&id)).unwrap();
    assert_eq!(done.status, Status::Completed, "{done:?}");
    assert_eq!(done.output, Some(json!({"paid": true})));
}

#[test]
fn a_workflow_timeout_faults_on_tick() {
    let harness = Harness::new(
        "timeout:\n  after:\n    seconds: 30\n",
        r#"
  - pause:
      wait:
        seconds: 60
"#,
    );
    let id = harness.start(json!({}));
    let parked = run(harness.engine.run_once(at(5_000))).unwrap();
    assert_eq!(parked.processed, 1);
    let waiting = run(harness.store.get(&id)).unwrap();
    assert_eq!(waiting.status, Status::Waiting(WaitReason::Timer));

    // The wait is still in the future. The workflow deadline is not.
    harness.clock.set(at(5_000 + 30_000));
    let stepped = harness.drive();
    assert!(stepped >= 1);
    let faulted = run(harness.store.get(&id)).unwrap();
    assert_eq!(faulted.status, Status::Faulted, "{faulted:?}");
    let fault = faulted.fault.expect("workflow timeout");
    assert_eq!(
        fault.error_type(),
        "https://open-workflow-specification.org/spec/1.0.0/errors/timeout"
    );
    assert_eq!(fault.instance(), "/");
}

#[test]
fn a_dropped_signal_still_finishes_after_the_sweeper() {
    let harness = Harness::new(
        "",
        r#"
  - pause:
      wait:
        seconds: 5
  - after:
      set:
        swept: true
"#,
    );
    harness.wakeup.drop_all();
    let id = harness.start(json!({}));
    let parked = run(harness.engine.run_once(harness.clock.now())).unwrap();
    assert_eq!(parked.processed, 1);
    let waiting = run(harness.store.get(&id)).unwrap();
    assert_eq!(waiting.status, Status::Waiting(WaitReason::Timer));
    assert!(
        waiting.lease.is_none(),
        "the wait commit releases the lease"
    );

    // The wait is not due yet, so a claim takes nothing. Arm a lease at the
    // deadline, then move the clock past it without stepping. `notify_due`
    // reads the store's sweep; the bus drops the hint it sends.
    let claimed =
        run(harness
            .store
            .claim(harness.engine.worker(), at(1_000), at(1_000 + 30_000), 1))
        .unwrap();
    assert!(
        claimed.is_empty(),
        "the wait is not due yet, so claim takes nothing"
    );
    harness.clock.set(waiting.wake_at.expect("wait"));
    let held = run(harness.store.claim(
        harness.engine.worker(),
        harness.clock.now(),
        at(harness.clock.now().millis() + 30_000),
        1,
    ))
    .unwrap();
    assert_eq!(held.len(), 1);
    let lease_until = held[0].lease.as_ref().expect("claimed lease").until;
    harness.clock.set(at(lease_until.millis() + 1));
    let swept = block_on(tideloom_runtime::notify_due(
        &harness.engine,
        harness.clock.now(),
    ));
    assert!(swept.is_ok());
    assert!(
        harness.wakeup.notifies() > 0,
        "the sweeper still notifies; the bus drops the signal"
    );
    assert!(
        harness.wakeup.dropped(),
        "the test drops every signal before the next claim"
    );
    let done = harness.drive();
    assert!(
        done >= 1,
        "the sweeper returns the lapsed lease to Runnable"
    );
    let stored = run(harness.store.get(&id)).unwrap();
    assert_eq!(stored.status, Status::Completed, "{stored:?}");
    assert_eq!(
        stored
            .output
            .as_ref()
            .and_then(|output| output.get("swept")),
        Some(&json!(true))
    );
}

#[test]
fn two_engines_do_not_run_the_same_step() {
    let harness = Harness::new(
        "",
        r#"
  - once:
      set:
        n: 1
"#,
    );
    let id = harness.start(json!({}));
    let now = harness.clock.now();
    run(async {
        let claimed = harness
            .store
            .claim(harness.engine.worker(), now, at(now.millis() + 30_000), 1)
            .await
            .unwrap();
        let right = harness
            .store
            .claim(harness.other.worker(), now, at(now.millis() + 30_000), 1)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1, "the first worker takes the instance");
        assert!(
            right.is_empty(),
            "the second worker must not claim the same step"
        );
        let conflict = harness.engine.step(claimed[0].clone(), now).await.unwrap();
        assert!(!conflict);
        // A second step of the same claim is a CAS loss, not a second run.
        // The first commit released the lease and completed the instance.
        let again = harness.other.step(claimed[0].clone(), now).await.unwrap();
        assert!(!again, "a terminal instance is dropped, not resumed");
    });
    let stored = run(harness.store.get(&id)).unwrap();
    assert_eq!(stored.status, Status::Completed, "{stored:?}");
    assert_eq!(stored.output, Some(json!({"n": 1})));
    assert_eq!(stored.version, 1, "one commit, not two");
    assert!(stored.lease.is_none());
}
