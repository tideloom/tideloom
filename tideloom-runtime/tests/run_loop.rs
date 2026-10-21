//! `Engine::run` against the in-memory backends.
//!
//! A signal starts a pass. Shutdown shortens this worker's leases and leaves
//! the instances `Running`, so the next `run_once` can sweep them. A future
//! `wake_at` is reached by moving `ManualClock`, not by sleeping until it.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tideloom_core::Definition;
use tideloom_core::Timestamp;
use tideloom_runtime::ActivityError;
use tideloom_runtime::ActivityExecutor;
use tideloom_runtime::Clock;
use tideloom_runtime::DefinitionKey;
use tideloom_runtime::DefinitionSource;
use tideloom_runtime::Engine;
use tideloom_runtime::EngineConfig;
use tideloom_runtime::EngineParts;
use tideloom_runtime::EventSink;
use tideloom_runtime::EventSource;
use tideloom_runtime::Hint;
use tideloom_runtime::InFlight;
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
use tideloom_runtime::WaitReason;
use tideloom_runtime::Wakeup;
use tideloom_runtime::WorkerId;
use tokio_util::sync::CancellationToken;

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn definition() -> Definition {
    Definition::from_yaml(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: run-loop\n  version: '0.1.0'\ndo:\n  - pause:\n      wait:\n        seconds: 30\n  - after:\n      set:\n        done: true\n",
    )
    .unwrap_or_else(|error| panic!("{error}"))
}

struct Harness {
    engine: Engine,
    store: Arc<MemoryStore>,
    wakeup: Arc<LocalWakeup>,
    clock: Arc<ManualClock>,
    key: DefinitionKey,
}

impl Harness {
    fn new() -> Self {
        let definitions = Arc::new(MemoryDefinitions::new());
        let key = definitions.publish(definition());
        let store = Arc::new(MemoryStore::new());
        let wakeup = Arc::new(LocalWakeup::new(NonZeroUsize::new(32).expect("capacity")));
        let clock = Arc::new(ManualClock::at(at(1_000)));
        let bus = Arc::new(LocalEventBus::new());
        let engine = engine_for("worker-a", &store, &wakeup, &definitions, &bus, &clock);
        Self {
            engine,
            store,
            wakeup,
            clock,
            key,
        }
    }

    fn start(&self) -> InstanceId {
        start_instance(&self.store, &self.key, &self.clock)
    }
}

fn start_instance(store: &MemoryStore, key: &DefinitionKey, clock: &ManualClock) -> InstanceId {
    let instance = Instance::new(key.clone(), None, json!({}), clock.now());
    let id = instance.id.clone();
    store
        .create(instance)
        .now_or_never()
        .expect("create is ready")
        .expect("create");
    id
}

fn engine_for(
    worker: &str,
    store: &Arc<MemoryStore>,
    wakeup: &Arc<LocalWakeup>,
    definitions: &Arc<MemoryDefinitions>,
    bus: &Arc<LocalEventBus>,
    clock: &Arc<ManualClock>,
) -> Engine {
    Engine::new(EngineParts {
        worker: WorkerId(worker.to_string()),
        store: Arc::clone(store) as Arc<dyn InstanceStore>,
        wakeup: Arc::clone(wakeup) as Arc<dyn Wakeup>,
        definitions: Arc::clone(definitions) as Arc<dyn DefinitionSource>,
        executors: vec![Arc::new(ReadyHttp)],
        events_out: Arc::clone(bus) as Arc<dyn EventSink>,
        events_in: Arc::clone(bus) as Arc<dyn EventSource>,
        clock: Arc::clone(clock) as Arc<dyn Clock>,
    })
}

/// An HTTP executor that returns immediately. The signal test never reaches it.
struct ReadyHttp;

#[async_trait]
impl ActivityExecutor for ReadyHttp {
    fn kind(&self) -> TaskKind {
        TaskKind::Http
    }

    async fn execute(&self, _task: TaskExecution) -> Result<serde_json::Value, ActivityError> {
        Ok(json!({"ready": true}))
    }
}

/// Holds an activity open so shutdown still has a lease to shorten.
///
/// A `set` or a `wait` releases its lease in the same pass. This executor
/// parks inside `execute`, after the engine has committed `Pending`.
struct IdleHttp {
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl IdleHttp {
    fn pair() -> (
        Self,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        (
            Self {
                started: Mutex::new(Some(started_tx)),
                release: Mutex::new(Some(release_rx)),
            },
            started_rx,
            release_tx,
        )
    }
}

#[async_trait]
impl ActivityExecutor for IdleHttp {
    fn kind(&self) -> TaskKind {
        TaskKind::Http
    }

    async fn execute(&self, _task: TaskExecution) -> Result<serde_json::Value, ActivityError> {
        let started = self.started.lock().expect("started").take();
        let release = self.release.lock().expect("release").take();
        if let Some(started) = started {
            let _ = started.send(());
        }
        if let Some(release) = release {
            let _ = release.await;
        }
        Ok(json!({"idle": true}))
    }
}

fn config() -> EngineConfig {
    EngineConfig {
        lease: Duration::from_secs(30),
        claim_limit: 4,
        // Longer than the test. A signal, not this interval, starts the pass.
        sweep_every: Duration::from_secs(60),
        poll_every: Duration::from_millis(20),
    }
}

#[tokio::test]
async fn a_signal_starts_a_pass_that_parks_the_wait() {
    let harness = Harness::new();
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let store = Arc::clone(&harness.store);
    let key = harness.key.clone();
    let clock = Arc::clone(&harness.clock);
    let engine = harness.engine;
    let running = tokio::spawn(async move { engine.run(config(), stop).await });
    // The loop's first pass has nothing to claim. The instance is created
    // after that, so the signal is what starts the pass that parks it.
    tokio::task::yield_now().await;
    let id = start_instance(&store, &key, &clock);

    harness
        .wakeup
        .notify(id.clone(), Hint::Other)
        .await
        .expect("notify");

    let parked = wait_until(|| {
        let store = Arc::clone(&harness.store);
        let id = id.clone();
        async move {
            let instance = store.get(&id).await.expect("get");
            instance.status == Status::Waiting(WaitReason::Timer)
        }
    })
    .await;
    assert!(
        parked,
        "the signal should claim the instance and park the wait"
    );
    let waiting = harness.store.get(&id).await.expect("get");
    assert_eq!(waiting.wake_at, Some(at(31_000)));
    assert!(
        waiting.lease.is_none(),
        "a parked wait does not hold a lease"
    );

    shutdown.cancel();
    running.await.expect("join").expect("run");
}

#[tokio::test]
async fn shutdown_waits_for_the_open_pass_then_shortens_a_lease_the_pass_still_holds() {
    let definitions = Arc::new(MemoryDefinitions::new());
    let key = definitions.publish(http_then_set());
    let store = Arc::new(MemoryStore::new());
    let wakeup = Arc::new(LocalWakeup::new(NonZeroUsize::new(8).expect("capacity")));
    let clock = Arc::new(ManualClock::at(at(5_000)));
    let bus = Arc::new(LocalEventBus::new());
    let (executor, started, release) = IdleHttp::pair();
    let engine = Engine::new(EngineParts {
        worker: WorkerId("worker-a".into()),
        store: Arc::clone(&store) as Arc<dyn InstanceStore>,
        wakeup: Arc::clone(&wakeup) as Arc<dyn Wakeup>,
        definitions: Arc::clone(&definitions) as Arc<dyn DefinitionSource>,
        executors: vec![Arc::new(executor)],
        events_out: Arc::clone(&bus) as Arc<dyn EventSink>,
        events_in: Arc::clone(&bus) as Arc<dyn EventSource>,
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
    });
    let mut instance = Instance::new(key.clone(), None, json!({}), clock.now());
    // A second instance is already claimed by this worker and stays `Running`
    // with `in_flight` set. Shutdown must shorten that lease and must not
    // flip it to `Runnable`: the next sweep is what recovers the open activity.
    let mut parked = instance_running("still-held", &WorkerId("worker-a".into()), at(90_000));
    parked.definition = key.clone();
    parked.in_flight = Some(InFlight {
        key: tideloom_core::TaskKey::new("/do/0", tideloom_core::ExecutionKey::empty()),
        attempt: 1,
    });
    parked.wake_at = Some(at(90_000));
    let held_id = parked.id.clone();
    let id = instance.id.clone();
    // The runnable row is claimed by the loop. The held row is not claimable:
    // its wake_at is in the future and it already has a live lease.
    instance.wake_at = Some(clock.now());
    store.create(instance).await.expect("create");
    store.create(parked).await.expect("create held");

    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let running = tokio::spawn(async move { engine.run(config(), stop).await });

    started
        .await
        .expect("the activity should open before shutdown");
    let open = store.get(&id).await.expect("get");
    assert_eq!(open.status, Status::Running);
    assert!(
        open.in_flight.is_some(),
        "Pending is committed before execute"
    );

    // The pass is inside the executor. Cancelling first must not abandon it:
    // `run` stays pending until the executor returns and the pass applies.
    let shutdown_at = clock.now();
    shutdown.cancel();
    tokio::task::yield_now().await;
    assert!(
        !running.is_finished(),
        "shutdown returned while the open activity's pass was still running"
    );
    release.send(()).expect("release the executor");
    running
        .await
        .expect("join")
        .expect("run returns after the pass");

    let released = store.get(&held_id).await.expect("get held");
    assert_eq!(
        released.status,
        Status::Running,
        "shutdown does not flip a held instance to Runnable"
    );
    assert!(
        released.in_flight.is_some(),
        "in_flight survives the lease shortening"
    );
    let lease = released.lease.expect("the lease is shortened, not cleared");
    assert_eq!(lease.worker, WorkerId("worker-a".into()));
    assert!(
        lease.until <= shutdown_at,
        "until {} should be no later than shutdown {}",
        lease.until.millis(),
        shutdown_at.millis()
    );

    // Sweep lists a lease strictly before `now`. The clock is still at the
    // shutdown instant, so move one millis and let the other worker sweep.
    clock.set(at(shutdown_at.millis() + 1));
    let other = engine_for("worker-b", &store, &wakeup, &definitions, &bus, &clock);
    let swept = other.run_once().await.expect("sweep pass");
    assert!(
        swept.processed >= 1,
        "the shortened lease is swept and claimed"
    );
    let recovered = store.get(&held_id).await.expect("get held");
    // The held row had `in_flight` and no Pending journal row is required for
    // the sweep itself. What shutdown must not do is flip the row to Runnable
    // and clear `in_flight` before this pass. The sweeper returns it to
    // Runnable, then this claim runs it; either way it left worker-a's lease.
    assert_ne!(
        recovered.lease.as_ref().map(|lease| &lease.worker),
        Some(&WorkerId("worker-a".into()))
    );
    assert!(
        recovered.lease.is_none()
            || recovered
                .lease
                .as_ref()
                .is_some_and(|lease| lease.worker != WorkerId("worker-a".into())),
        "worker-a's shortened lease does not survive the sweep"
    );
}

#[tokio::test]
async fn a_lost_signal_still_runs_on_the_sweep_interval() {
    let harness = Harness::new();
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let store = Arc::clone(&harness.store);
    let key = harness.key.clone();
    let clock = Arc::clone(&harness.clock);
    let engine = harness.engine;
    // No notify. The sweep interval is the backstop, and the runtime clock is
    // paused so the interval does not fire until the test advances it.
    tokio::time::pause();
    let running = tokio::spawn(async move {
        engine
            .run(
                EngineConfig {
                    sweep_every: Duration::from_millis(50),
                    ..config()
                },
                stop,
            )
            .await
    });
    // Let the first pass run while the store is empty, then create the
    // instance. The sweep tick is what claims it.
    tokio::task::yield_now().await;
    let id = start_instance(&store, &key, &clock);

    tokio::time::advance(Duration::from_millis(50)).await;
    let parked =
        wait_until(|| {
            let store = Arc::clone(&harness.store);
            let id = id.clone();
            async move {
                store.get(&id).await.expect("get").status == Status::Waiting(WaitReason::Timer)
            }
        })
        .await;
    assert!(
        parked,
        "the sweep interval should claim a runnable instance with no signal"
    );

    shutdown.cancel();
    tokio::time::resume();
    running.await.expect("join").expect("run");
}

#[tokio::test]
async fn a_due_timer_is_claimed_by_moving_the_clock_not_by_sleeping_until_it() {
    let harness = Harness::new();
    let id = harness.start();
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let engine = harness.engine;
    let running = tokio::spawn(async move { engine.run(config(), stop).await });

    harness
        .wakeup
        .notify(id.clone(), Hint::Timer)
        .await
        .expect("notify");
    let parked =
        wait_until(|| {
            let store = Arc::clone(&harness.store);
            let id = id.clone();
            async move {
                store.get(&id).await.expect("get").status == Status::Waiting(WaitReason::Timer)
            }
        })
        .await;
    assert!(parked);

    let started = std::time::Instant::now();
    // 30s in workflow time. Sleeping until wake_at would blow the test bound.
    harness.clock.set(at(31_000));
    let done = wait_until(|| {
        let store = Arc::clone(&harness.store);
        let id = id.clone();
        async move { store.get(&id).await.expect("get").status == Status::Completed }
    })
    .await;
    assert!(done, "moving the clock to wake_at should release the wait");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the loop slept toward wake_at instead of polling: {:?}",
        started.elapsed()
    );
    let output = harness.store.get(&id).await.expect("get").output;
    assert_eq!(
        output.as_ref().and_then(|value| value.get("done")),
        Some(&json!(true))
    );

    shutdown.cancel();
    running.await.expect("join").expect("run");
}

#[tokio::test]
async fn release_leases_leaves_another_workers_lease_and_an_already_lapsed_one() {
    let store = MemoryStore::new();
    let mine = WorkerId("worker-a".into());
    let theirs = WorkerId("worker-b".into());
    let mut held = instance_running("held", &mine, at(9_000));
    held.in_flight = Some(InFlight {
        key: tideloom_core::TaskKey::new("/do/0", tideloom_core::ExecutionKey::empty()),
        attempt: 1,
    });
    let other = instance_running("other", &theirs, at(9_000));
    let lapsed = instance_running("lapsed", &mine, at(1_000));
    store.create(held).await.expect("create");
    store.create(other).await.expect("create");
    store.create(lapsed).await.expect("create");

    let changed = store
        .release_leases(&mine, at(5_000))
        .await
        .expect("release");
    assert_eq!(changed, 1);

    let held = store
        .get(&InstanceId::from_string("held"))
        .await
        .expect("get");
    assert_eq!(held.status, Status::Running);
    assert!(held.in_flight.is_some());
    assert_eq!(held.lease.expect("lease").until, at(5_000));

    let other = store
        .get(&InstanceId::from_string("other"))
        .await
        .expect("get");
    let other_lease = other.lease.expect("lease");
    assert_eq!(other_lease.until, at(9_000));
    assert_eq!(other_lease.worker, theirs);

    let lapsed = store
        .get(&InstanceId::from_string("lapsed"))
        .await
        .expect("get");
    assert_eq!(lapsed.lease.expect("lease").until, at(1_000));
}

fn instance_running(id: &str, worker: &WorkerId, until: Timestamp) -> Instance {
    let mut instance = Instance::new(
        DefinitionKey {
            namespace: "test".into(),
            name: "lease".into(),
            version: "1".into(),
            content_hash: "pinned".into(),
        },
        None,
        json!({}),
        at(0),
    );
    instance.id = InstanceId::from_string(id);
    instance.status = Status::Running;
    instance.lease = Some(tideloom_runtime::Lease {
        worker: worker.clone(),
        until,
    });
    instance
}

fn http_then_set() -> Definition {
    Definition::from_yaml(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: open-http\n  version: '0.1.0'\ndo:\n  - call:\n      call: http\n      with:\n        method: GET\n        endpoint: http://127.0.0.1:9/idle\n",
    )
    .unwrap_or_else(|error| panic!("{error}"))
}

async fn wait_until<F, Fut>(mut probe: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if probe().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::task::yield_now().await;
    }
}

trait NowOrNever: Future {
    fn now_or_never(self) -> Option<Self::Output>
    where
        Self: Sized,
    {
        let mut future = std::pin::pin!(self);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(output) => Some(output),
            std::task::Poll::Pending => None,
        }
    }
}

impl<F: Future> NowOrNever for F {}
