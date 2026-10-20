//! Journal commits around an activity, and the crash decision that reads them.
//!
//! A finished call upserts one row from `Pending` to `Output`. A crash between
//! those commits leaves `Pending`. The next claim re-runs only an idempotent
//! executor, at `attempt` 1; otherwise the instance is faulted and the open
//! attempt is recorded as `Unknown` on the next attempt number.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use async_trait::async_trait;
use serde_json::Value;
use serde_json::json;
use tideloom_core::Definition;
use tideloom_core::ExecutionKey;
use tideloom_core::ListenSpec;
use tideloom_core::TaskKey;
use tideloom_core::Timestamp;
use tideloom_runtime::ActivityError;
use tideloom_runtime::ActivityExecutor;
use tideloom_runtime::CloudEvent;
use tideloom_runtime::Commit;
use tideloom_runtime::DefinitionKey;
use tideloom_runtime::Engine;
use tideloom_runtime::EngineParts;
use tideloom_runtime::EventError;
use tideloom_runtime::EventSink;
use tideloom_runtime::EventSource;
use tideloom_runtime::InFlight;
use tideloom_runtime::Instance;
use tideloom_runtime::InstanceId;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::ManualClock;
use tideloom_runtime::MemoryDefinitions;
use tideloom_runtime::MemoryStore;
use tideloom_runtime::Status;
use tideloom_runtime::StepOutcome;
use tideloom_runtime::StepResult;
use tideloom_runtime::TaskExecution;
use tideloom_runtime::TaskKind;
use tideloom_runtime::Wakeup;
use tideloom_runtime::WorkerId;

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn run<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    match pin!(future).as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("journal step unexpectedly suspended"),
    }
}

fn definition(tasks: &str) -> Definition {
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: journal\n  version: '0.1.0'\ndo:\n{tasks}"
    );
    Definition::from_yaml(&yaml).unwrap_or_else(|error| panic!("{error}"))
}

fn publish(definitions: &MemoryDefinitions, tasks: &str) -> DefinitionKey {
    definitions.publish(definition(tasks))
}

fn instance(id: &str, definition: DefinitionKey) -> Instance {
    let mut instance = Instance::new(definition, None, json!({}), at(1));
    instance.id = InstanceId::from_string(id);
    instance
}

struct Silent;

#[async_trait]
impl Wakeup for Silent {
    async fn notify(
        &self,
        _: InstanceId,
        _: tideloom_runtime::Hint,
    ) -> Result<tideloom_runtime::Offset, tideloom_runtime::BusError> {
        Ok(tideloom_runtime::Offset(0))
    }
    async fn watch(
        &self,
        _: tideloom_runtime::Offset,
    ) -> Result<tideloom_runtime::SignalStream, tideloom_runtime::BusError> {
        Err(tideloom_runtime::BusError::Backend(
            "journal tests do not watch".into(),
        ))
    }
    async fn commit_offset(
        &self,
        _: tideloom_runtime::Offset,
    ) -> Result<(), tideloom_runtime::BusError> {
        Ok(())
    }
    async fn committed(&self) -> Result<tideloom_runtime::Offset, tideloom_runtime::BusError> {
        Ok(tideloom_runtime::Offset(0))
    }
}

#[async_trait]
impl EventSink for Silent {
    async fn publish(&self, _: CloudEvent) -> Result<Vec<InstanceId>, EventError> {
        Ok(Vec::new())
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

struct CountingExecutor {
    kind: TaskKind,
    idempotent: bool,
    hits: AtomicUsize,
    output: Value,
}

impl CountingExecutor {
    fn new(kind: TaskKind, idempotent: bool) -> Self {
        Self {
            kind,
            idempotent,
            hits: AtomicUsize::new(0),
            output: json!({"sent": true}),
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ActivityExecutor for CountingExecutor {
    fn kind(&self) -> TaskKind {
        self.kind
    }

    fn idempotent(&self) -> bool {
        self.idempotent
    }

    async fn execute(&self, task: TaskExecution) -> Result<Value, ActivityError> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        let _ = task;
        Ok(self.output.clone())
    }
}

fn engine(
    store: &Arc<MemoryStore>,
    definitions: &Arc<MemoryDefinitions>,
    executors: Vec<Arc<dyn ActivityExecutor>>,
    clock: &Arc<ManualClock>,
) -> Engine {
    Engine::new(EngineParts {
        worker: WorkerId("journal".into()),
        store: store.clone(),
        wakeup: Arc::new(Silent),
        definitions: definitions.clone(),
        executors,
        events_out: Arc::new(Silent),
        events_in: Arc::new(Silent),
        clock: Arc::clone(clock) as Arc<dyn tideloom_runtime::Clock>,
    })
}

/// The fault detail a non-idempotent crash writes. The engine keeps this
/// string crate-private; the assertion is the observable fault text.
const UNKNOWN_DETAIL: &str = "activity outcome unknown";

/// Commit the `Pending` row a dead worker would have written, without running
/// the executor. The engine no longer exposes a cut between those two commits;
/// the store is the persistence boundary the next claim reads. `task` is the
/// first task name in the published document, which is `/do/0/{task}`.
fn leave_pending(
    store: &MemoryStore,
    claimed: &Instance,
    now: Timestamp,
    kind: TaskKind,
    task: &str,
) {
    let key = TaskKey::new(format!("/do/0/{task}"), ExecutionKey::new([]));
    let mut commit = Commit::new(claimed.id.clone(), claimed.version, Status::Running, now);
    commit.continuation = claimed.continuation.clone();
    commit.release_lease = false;
    commit.in_flight = Some(InFlight {
        key: key.clone(),
        attempt: 0,
    });
    commit.step_results.push(StepResult {
        instance: claimed.id.clone(),
        key,
        kind,
        attempt: 0,
        input: json!({}),
        outcome: StepOutcome::Pending,
        started_at: now,
        finished_at: now,
    });
    run(store.apply(commit)).unwrap_or_else(|error| panic!("{error}"));
}

fn claim_one(store: &MemoryStore, now: Timestamp) -> Instance {
    let claimed = run(store.claim(
        &WorkerId("journal".into()),
        now,
        Timestamp::from_millis(now.millis() + 30_000),
        1,
    ))
    .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(claimed.len(), 1, "expected one claimable instance");
    claimed.into_iter().next().unwrap_or_else(|| unreachable!())
}

#[test]
fn an_http_call_upserts_pending_into_output_on_the_same_attempt() {
    let server =
        std::net::TcpListener::bind("127.0.0.1:0").unwrap_or_else(|error| panic!("{error}"));
    let port = server
        .local_addr()
        .unwrap_or_else(|error| panic!("{error}"))
        .port();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = Arc::clone(&hits);
    std::thread::Builder::new()
        .name("journal-http".into())
        .spawn(move || {
            for incoming in server.incoming() {
                let Ok(mut stream) = incoming else {
                    break;
                };
                hits_thread.fetch_add(1, Ordering::SeqCst);
                let mut buffer = [0; 1024];
                let _ = std::io::Read::read(&mut stream, &mut buffer);
                let body = b"{\"sent\":true}";
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
                let _ = std::io::Write::write_all(&mut stream, body);
            }
        })
        .unwrap_or_else(|error| panic!("{error}"));

    let definitions = Arc::new(MemoryDefinitions::new());
    let key = publish(
        &definitions,
        &format!(
            "  - notify:\n      call: http\n      with:\n        method: POST\n        endpoint: http://127.0.0.1:{port}/paint\n"
        ),
    );
    let store = Arc::new(MemoryStore::new());
    let clock = Arc::new(ManualClock::at(at(1_000)));
    let engine = engine(
        &store,
        &definitions,
        vec![Arc::new(tideloom_runtime::HttpExecutor::new(
            Arc::clone(&definitions) as Arc<dyn tideloom_runtime::DefinitionSource>,
        ))],
        &clock,
    );
    run(store.create(instance("http", key))).unwrap_or_else(|error| panic!("{error}"));

    let now = at(1_000);
    let claimed = claim_one(&store, now);
    leave_pending(&store, &claimed, now, TaskKind::Http, "notify");
    let pending = run(store.steps(&InstanceId::from_string("http"))).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].attempt, 0);
    assert_eq!(pending[0].outcome, StepOutcome::Pending);
    assert_eq!(pending[0].kind, TaskKind::Http);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "pending commit must precede the call"
    );
    let held = run(store.get(&InstanceId::from_string("http"))).unwrap();
    assert_eq!(held.status, Status::Running);
    assert!(
        held.in_flight.is_some(),
        "the open attempt is the instance's in_flight, not a wait reason"
    );
    assert!(
        held.lease.is_some(),
        "the lease stays held across the two commits"
    );

    run(engine.step(held, now)).unwrap_or_else(|error| panic!("{error}"));
    let done = run(store.steps(&InstanceId::from_string("http"))).unwrap();
    assert_eq!(
        done.len(),
        1,
        "Pending and Output share one row: Commit upserts by (instance, key, attempt)"
    );
    assert_eq!(done[0].attempt, 0);
    assert_eq!(done[0].outcome, StepOutcome::Output(json!({"sent": true})));
    assert_eq!(done[0].started_at, now);
    assert!(done[0].finished_at >= done[0].started_at);
    let finished = run(store.get(&InstanceId::from_string("http"))).unwrap();
    assert_eq!(finished.status, Status::Completed);
    assert_eq!(finished.output, Some(json!({"sent": true})));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn a_crash_between_the_two_commits_faults_a_non_idempotent_http_call() {
    let definitions = Arc::new(MemoryDefinitions::new());
    let key = publish(
        &definitions,
        "  - notify:\n      call: http\n      with:\n        method: POST\n        endpoint: http://127.0.0.1:9/never\n",
    );
    let store = Arc::new(MemoryStore::new());
    // Unregistered: the crash is the Pending row, not a second request. A
    // registered `HttpExecutor` is also non-idempotent, so recovery faults
    // the same way once the row exists.
    let clock = Arc::new(ManualClock::at(at(1_000)));
    let engine = engine(&store, &definitions, Vec::new(), &clock);
    let id = InstanceId::from_string("crashed-http");
    run(store.create(instance("crashed-http", key))).unwrap();

    let now = at(2_000);
    let claimed = claim_one(&store, now);
    leave_pending(&store, &claimed, now, TaskKind::Http, "notify");
    // The worker died here: Pending is committed, the call was not made, the
    // lease is still held. Advance past that lease; run_once sweeps the
    // Running row and claims it again.
    let crashed = run(store.get(&id)).unwrap();
    let until = crashed.lease.expect("held lease").until;
    clock.set(at(until.millis() + 1));
    let recovered = run(engine.run_once()).unwrap();
    assert!(
        recovered.processed >= 1,
        "the lapsed instance must be claimed again"
    );
    let finished = run(store.get(&id)).unwrap();
    assert_eq!(finished.status, Status::Faulted);
    let fault = finished.fault.unwrap_or_else(|| panic!("missing fault"));
    assert_eq!(fault.detail(), Some(UNKNOWN_DETAIL));
    assert!(
        fault.error_type().contains("runtime"),
        "fault type {}",
        fault.error_type()
    );

    let steps = run(store.steps(&id)).unwrap();
    assert!(
        steps
            .iter()
            .any(|step| step.outcome == StepOutcome::Unknown),
        "journal should record Unknown, got {steps:?}"
    );
    assert!(
        steps
            .iter()
            .any(|step| matches!(step.outcome, StepOutcome::Pending) && step.attempt == 0),
        "the crashed attempt stays Pending; Unknown is the next attempt"
    );
}

#[test]
fn a_crash_between_the_two_commits_reruns_an_idempotent_executor_at_attempt_one() {
    let definitions = Arc::new(MemoryDefinitions::new());
    let key = publish(
        &definitions,
        "  - note:\n      emit:\n        event:\n          with:\n            source: https://example.test/journal\n            type: io.example.noted\n",
    );
    let store = Arc::new(MemoryStore::new());
    let executor = Arc::new(CountingExecutor::new(TaskKind::Emit, true));
    let clock = Arc::new(ManualClock::at(at(1_000)));
    let engine = engine(&store, &definitions, vec![executor.clone()], &clock);
    let id = InstanceId::from_string("retry-emit");
    run(store.create(instance("retry-emit", key))).unwrap();

    let now = at(3_000);
    let claimed = claim_one(&store, now);
    leave_pending(&store, &claimed, now, TaskKind::Emit, "note");
    assert_eq!(executor.hits(), 0);
    let crashed = run(store.get(&id)).unwrap();
    assert_eq!(crashed.status, Status::Running);
    let until = crashed.lease.expect("held lease").until;

    clock.set(at(until.millis() + 1));
    let recovered = run(engine.run_once()).unwrap();
    assert!(recovered.processed >= 1);
    assert_eq!(
        executor.hits(),
        1,
        "the idempotent executor runs on recovery"
    );
    let finished = run(store.get(&id)).unwrap();
    assert_eq!(finished.status, Status::Completed, "{finished:?}");

    let steps = run(store.steps(&id)).unwrap();
    let retried = steps
        .iter()
        .find(|step| step.attempt == 1)
        .unwrap_or_else(|| panic!("missing attempt 1: {steps:?}"));
    assert!(matches!(retried.outcome, StepOutcome::Output(_)));
    assert!(
        steps
            .iter()
            .any(|step| step.attempt == 0 && step.outcome == StepOutcome::Pending),
        "attempt 0 stays Pending because the outcome upserts its own attempt"
    );
}

#[test]
fn a_crash_between_the_two_commits_faults_a_non_idempotent_executor() {
    let definitions = Arc::new(MemoryDefinitions::new());
    let key = publish(
        &definitions,
        "  - note:\n      emit:\n        event:\n          with:\n            source: https://example.test/journal\n            type: io.example.noted\n",
    );
    let store = Arc::new(MemoryStore::new());
    let executor = Arc::new(CountingExecutor::new(TaskKind::Emit, false));
    let clock = Arc::new(ManualClock::at(at(1_000)));
    let engine = engine(&store, &definitions, vec![executor.clone()], &clock);
    let id = InstanceId::from_string("once-emit");
    run(store.create(instance("once-emit", key))).unwrap();

    let now = at(4_000);
    let claimed = claim_one(&store, now);
    leave_pending(&store, &claimed, now, TaskKind::Emit, "note");
    let crashed = run(store.get(&id)).unwrap();
    let until = crashed.lease.as_ref().expect("held lease").until;

    clock.set(at(until.millis() + 1));
    let recovered = run(engine.run_once()).unwrap();
    assert!(recovered.processed >= 1);
    assert_eq!(
        executor.hits(),
        0,
        "a non-idempotent executor is not run again"
    );
    let finished = run(store.get(&id)).unwrap();
    assert_eq!(finished.status, Status::Faulted);
    assert_eq!(
        finished.fault.as_ref().and_then(|fault| fault.detail()),
        Some(UNKNOWN_DETAIL)
    );
    let steps = run(store.steps(&id)).unwrap();
    assert!(
        steps
            .iter()
            .any(|step| { step.outcome == StepOutcome::Unknown && step.attempt == 1 })
    );
}
