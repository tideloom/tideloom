//! Crash recovery through two engines sharing a real PostgreSQL store.
//!
//! Worker A is aborted inside its HTTP executor, after the engine committed
//! `Pending`. Worker B sweeps the strictly expired lease and either records an
//! unknown outcome or reruns the activity, according to executor idempotence.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use serde_json::json;
use tideloom_core::Definition;
use tideloom_core::Timestamp;
use tideloom_postgres::PgStore;
use tideloom_runtime::ActivityError;
use tideloom_runtime::ActivityExecutor;
use tideloom_runtime::Clock;
use tideloom_runtime::DefinitionSource;
use tideloom_runtime::Engine;
use tideloom_runtime::EngineParts;
use tideloom_runtime::EventSink;
use tideloom_runtime::EventSource;
use tideloom_runtime::Instance;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::LocalEventBus;
use tideloom_runtime::LocalWakeup;
use tideloom_runtime::ManualClock;
use tideloom_runtime::MemoryDefinitions;
use tideloom_runtime::Status;
use tideloom_runtime::StepOutcome;
use tideloom_runtime::StepResult;
use tideloom_runtime::TaskExecution;
use tideloom_runtime::TaskKind;
use tideloom_runtime::Wakeup;
use tideloom_runtime::WorkerId;
use uuid::Uuid;

const UNKNOWN_DETAIL: &str = "activity outcome unknown";

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn database_available() -> bool {
    if std::env::var_os("TIDELOOM_TEST_DATABASE_URL").is_none() {
        eprintln!("skipping crash recovery tests: TIDELOOM_TEST_DATABASE_URL is unset");
        return false;
    }
    true
}

struct Database {
    url: String,
    name: String,
}

impl Database {
    async fn create() -> Self {
        let url = std::env::var("TIDELOOM_TEST_DATABASE_URL").unwrap_or_else(|_| {
            panic!("TIDELOOM_TEST_DATABASE_URL is required once a test has started")
        });
        let name = format!("s6_{}", Uuid::now_v7().simple());
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("admin connection");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("create database");
        admin.close().await;
        let test_url = replace_database(&url, &name);
        let store = PgStore::connect(&test_url).await.expect("connect");
        store.migrate().await.expect("migrate");
        Self {
            url: test_url,
            name,
        }
    }

    async fn store(&self) -> PgStore {
        PgStore::connect(&self.url).await.expect("connect store")
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let url = std::env::var("TIDELOOM_TEST_DATABASE_URL").expect("url");
        let name = self.name.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let admin = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&url)
                    .await
                    .expect("admin connection");
                let _ = sqlx::query(&format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{name}'"
                ))
                .execute(&admin)
                .await;
                let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name}"))
                    .execute(&admin)
                    .await;
                admin.close().await;
            });
        })
        .join()
        .expect("drop database");
    }
}

fn replace_database(url: &str, name: &str) -> String {
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let prefix = base.rfind('/').expect("database segment");
    let mut replaced = format!("{}{name}", &base[..=prefix]);
    if !query.is_empty() {
        replaced.push('?');
        replaced.push_str(query);
    }
    replaced
}

struct BlockingHttp {
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl BlockingHttp {
    fn new() -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        (
            Self {
                started: Mutex::new(Some(started_tx)),
            },
            started_rx,
        )
    }
}

#[async_trait]
impl ActivityExecutor for BlockingHttp {
    fn kind(&self) -> TaskKind {
        TaskKind::Http
    }

    async fn execute(&self, _task: TaskExecution) -> Result<Value, ActivityError> {
        let started = self.started.lock().expect("started").take();
        if let Some(started) = started {
            let _ = started.send(());
        }
        std::future::pending().await
    }
}

struct RecoveryHttp {
    idempotent: bool,
    attempts: Mutex<Vec<u32>>,
}

#[async_trait]
impl ActivityExecutor for RecoveryHttp {
    fn kind(&self) -> TaskKind {
        TaskKind::Http
    }

    fn idempotent(&self) -> bool {
        self.idempotent
    }

    async fn execute(&self, task: TaskExecution) -> Result<Value, ActivityError> {
        self.attempts.lock().expect("attempts").push(task.attempt);
        Ok(json!({"recovered": true}))
    }
}

fn definition() -> Definition {
    Definition::from_yaml(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: crash-recovery\n  version: '0.1.0'\ndo:\n  - call:\n      call: http\n      with:\n        method: POST\n        endpoint: http://127.0.0.1:9/recover\n",
    )
    .unwrap_or_else(|error| panic!("{error}"))
}

fn engine(
    worker: &str,
    store: &Arc<PgStore>,
    wakeup: &Arc<LocalWakeup>,
    definitions: &Arc<MemoryDefinitions>,
    bus: &Arc<LocalEventBus>,
    clock: &Arc<ManualClock>,
    executor: Arc<dyn ActivityExecutor>,
) -> Engine {
    Engine::new(EngineParts {
        worker: WorkerId(worker.into()),
        store: Arc::clone(store) as Arc<dyn InstanceStore>,
        wakeup: Arc::clone(wakeup) as Arc<dyn Wakeup>,
        definitions: Arc::clone(definitions) as Arc<dyn DefinitionSource>,
        executors: vec![executor],
        events_out: Arc::clone(bus) as Arc<dyn EventSink>,
        events_in: Arc::clone(bus) as Arc<dyn EventSource>,
        clock: Arc::clone(clock) as Arc<dyn Clock>,
    })
}

struct Recovery {
    instance: Instance,
    steps: Vec<StepResult>,
    attempts: Vec<u32>,
}

async fn crash_and_recover(idempotent: bool) -> Recovery {
    let database = Database::create().await;
    let store = Arc::new(database.store().await);
    let definitions = Arc::new(MemoryDefinitions::new());
    let key = definitions.publish(definition());
    let wakeup = Arc::new(LocalWakeup::new(NonZeroUsize::new(8).expect("capacity")));
    let bus = Arc::new(LocalEventBus::new());
    let clock = Arc::new(ManualClock::at(at(1_000)));
    let (blocking, started) = BlockingHttp::new();
    let worker_a = engine(
        "worker-a",
        &store,
        &wakeup,
        &definitions,
        &bus,
        &clock,
        Arc::new(blocking),
    );
    let recovery_executor = Arc::new(RecoveryHttp {
        idempotent,
        attempts: Mutex::new(Vec::new()),
    });
    let worker_b = engine(
        "worker-b",
        &store,
        &wakeup,
        &definitions,
        &bus,
        &clock,
        recovery_executor.clone(),
    );
    let instance = Instance::new(key, None, json!({}), clock.now());
    let id = instance.id.clone();
    store.create(instance).await.expect("create");

    let running = tokio::spawn(async move { worker_a.run_once().await });
    started
        .await
        .expect("execute begins after Pending has been committed");
    let crashed = store.get(&id).await.expect("get crashed");
    assert_eq!(crashed.status, Status::Running);
    assert_eq!(crashed.in_flight.as_ref().map(|open| open.attempt), Some(0));
    let pending = store.steps(&id).await.expect("pending step");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].attempt, 0);
    assert_eq!(pending[0].outcome, StepOutcome::Pending);
    let until = crashed.lease.as_ref().expect("worker A lease").until;

    running.abort();
    let error = running.await.expect_err("worker A was aborted");
    assert!(error.is_cancelled());
    clock.set(at(until.millis() + 1));
    let recovered = worker_b.run_once().await.expect("recovery pass");
    assert_eq!(recovered.processed, 1, "worker B claims the swept instance");

    Recovery {
        instance: store.get(&id).await.expect("get recovered"),
        steps: store.steps(&id).await.expect("recovered steps"),
        attempts: recovery_executor.attempts.lock().expect("attempts").clone(),
    }
}

#[tokio::test]
async fn a_crashed_non_idempotent_http_call_is_unknown_and_is_not_executed_again() {
    if !database_available() {
        return;
    }
    let recovered = crash_and_recover(false).await;

    assert_eq!(recovered.attempts, Vec::<u32>::new());
    assert_eq!(recovered.instance.status, Status::Faulted);
    assert_eq!(
        recovered
            .instance
            .fault
            .as_ref()
            .and_then(|fault| fault.detail()),
        Some(UNKNOWN_DETAIL)
    );
    assert_eq!(recovered.steps.len(), 2);
    assert!(
        recovered
            .steps
            .iter()
            .any(|step| { step.attempt == 0 && step.outcome == StepOutcome::Pending })
    );
    assert!(
        recovered
            .steps
            .iter()
            .any(|step| { step.attempt == 1 && step.outcome == StepOutcome::Unknown })
    );
}

#[tokio::test]
async fn a_crashed_idempotent_http_call_reruns_only_attempt_one_and_completes() {
    if !database_available() {
        return;
    }
    let recovered = crash_and_recover(true).await;

    assert_eq!(recovered.attempts, vec![1]);
    assert_eq!(recovered.instance.status, Status::Completed);
    assert_eq!(recovered.instance.output, Some(json!({"recovered": true})));
    assert_eq!(recovered.steps.len(), 2);
    assert!(
        recovered
            .steps
            .iter()
            .any(|step| { step.attempt == 0 && step.outcome == StepOutcome::Pending })
    );
    assert!(recovered.steps.iter().any(|step| {
        step.attempt == 1 && step.outcome == StepOutcome::Output(json!({"recovered": true}))
    }));
}
