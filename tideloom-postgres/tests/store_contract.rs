//! The InstanceStore assertions MemoryStore already satisfies, run against PgStore.
//!
//! Each test gets its own database so a failed apply cannot leak rows into the
//! next case. The URL comes from `TIDELOOM_TEST_DATABASE_URL`; a missing URL
//! skips, which is how CI on macOS and Windows stays green without Postgres.

use std::sync::Arc;

use serde_json::json;
use tideloom_core::ExecutionKey;
use tideloom_core::ListenSpec;
use tideloom_core::TaskKey;
use tideloom_core::Timestamp;
use tideloom_postgres::PgStore;
use tideloom_runtime::CloudEvent;
use tideloom_runtime::Commit;
use tideloom_runtime::DefinitionKey;
use tideloom_runtime::Engine;
use tideloom_runtime::EngineError;
use tideloom_runtime::EngineParts;
use tideloom_runtime::EventError;
use tideloom_runtime::EventSink;
use tideloom_runtime::EventSource;
use tideloom_runtime::InFlight;
use tideloom_runtime::Instance;
use tideloom_runtime::InstanceId;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::Lease;
use tideloom_runtime::ManualClock;
use tideloom_runtime::MemoryDefinitions;
use tideloom_runtime::ParentRef;
use tideloom_runtime::SpawnChild;
use tideloom_runtime::Status;
use tideloom_runtime::StepOutcome;
use tideloom_runtime::StepResult;
use tideloom_runtime::StoreError;
use tideloom_runtime::TaskKind;
use tideloom_runtime::WaitReason;
use tideloom_runtime::Wakeup;
use tideloom_runtime::WorkerId;
use uuid::Uuid;

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn instance(created: u64) -> Instance {
    instance_with(Uuid::now_v7(), created)
}

fn instance_with(id: Uuid, created: u64) -> Instance {
    let mut instance = Instance::new(
        DefinitionKey {
            namespace: "test".into(),
            name: "memory".into(),
            version: "1".into(),
            content_hash: "pinned".into(),
        },
        None,
        json!({"input": id.to_string()}),
        at(created),
    );
    instance.id = InstanceId::from_string(id.to_string());
    instance
}

fn engine(store: &Arc<PgStore>) -> Engine {
    Engine::new(EngineParts {
        worker: WorkerId("worker".into()),
        store: store.clone(),
        wakeup: Arc::new(UnusedBackends),
        definitions: Arc::new(MemoryDefinitions::new()),
        executors: Vec::new(),
        events_out: Arc::new(UnusedBackends),
        events_in: Arc::new(UnusedBackends),
        clock: Arc::new(ManualClock::at(at(0))),
    })
}

fn step(id: &InstanceId, position: &str, attempt: u32, started: u64) -> StepResult {
    StepResult {
        instance: id.clone(),
        key: TaskKey::new(position, ExecutionKey::empty()),
        kind: TaskKind::Http,
        attempt,
        input: json!({"request": position}),
        outcome: StepOutcome::Pending,
        started_at: at(started),
        finished_at: at(started),
    }
}

fn child(parent: &Instance, branch: Option<usize>) -> SpawnChild {
    SpawnChild {
        id: InstanceId::from_string(Uuid::now_v7().to_string()),
        definition: parent.definition.clone(),
        parent: ParentRef {
            id: parent.id.clone(),
            branch,
        },
        input: json!({"child": true}),
        root_task: None,
    }
}

pub struct Schema {
    url: String,
    name: String,
}

pub async fn schema() -> Schema {
    Schema::create().await
}

pub fn bare_instance(created: u64) -> Instance {
    instance(created)
}

impl Schema {
    pub async fn store(&self) -> PgStore {
        PgStore::connect(&self.url).await.expect("connect")
    }

    async fn create() -> Self {
        let url = std::env::var("TIDELOOM_TEST_DATABASE_URL").unwrap_or_else(|_| {
            panic!("TIDELOOM_TEST_DATABASE_URL is required once a test has started")
        });
        let name = format!("s1_{}", Uuid::now_v7().simple());
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
}

impl Drop for Schema {
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

fn skip_without_database() -> bool {
    if std::env::var_os("TIDELOOM_TEST_DATABASE_URL").is_none() {
        eprintln!("skipping PgStore tests: TIDELOOM_TEST_DATABASE_URL is unset");
        return true;
    }
    false
}

#[tokio::test]
async fn creating_an_existing_id_does_not_replace_it_and_reads_are_snapshots() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = schema.store().await;
    let original = instance(10);
    store.create(original.clone()).await.unwrap();
    let mut duplicate = original.clone();
    duplicate.input = json!("replacement");
    assert!(matches!(
        store.create(duplicate).await,
        Err(StoreError::AlreadyExists(id)) if id == original.id
    ));
    let mut snapshot = store.get(&original.id).await.unwrap();
    snapshot.continuation = Some(vec![1, 2]);
    assert_eq!(store.get(&original.id).await.unwrap(), original);
    let missing = InstanceId::from_string(Uuid::now_v7().to_string());
    assert!(matches!(
        store.get(&missing).await,
        Err(StoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn claims_are_due_ordered_limited_and_exclude_live_leases_and_other_waits() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = schema.store().await;
    let worker = WorkerId("worker".into());
    let mut rows = Vec::new();
    for (created, status, wake, lease_until) in [
        (9, Status::Runnable, Some(10), None),
        (2, Status::Waiting(WaitReason::Timer), Some(10), None),
        (4, Status::Runnable, Some(4), Some(10)),
        (7, Status::Runnable, None, None),
        (1, Status::Runnable, Some(11), None),
        (1, Status::Waiting(WaitReason::Timer), Some(11), None),
        (1, Status::Waiting(WaitReason::Timer), None, None),
        (1, Status::Runnable, Some(1), Some(11)),
        (1, Status::Running, Some(1), Some(9)),
        (1, Status::Waiting(WaitReason::Join), Some(1), None),
        (1, Status::Waiting(WaitReason::Child), Some(1), None),
        (1, Status::Waiting(WaitReason::Events), Some(1), None),
        (1, Status::Completed, Some(1), None),
        (1, Status::Faulted, Some(1), None),
        (1, Status::Cancelled, Some(1), None),
    ] {
        let mut value = instance(created);
        value.status = status;
        value.wake_at = wake.map(at);
        value.lease = lease_until.map(|until| Lease {
            worker: WorkerId("old".into()),
            until: at(until),
        });
        store.create(value.clone()).await.unwrap();
        rows.push(value);
    }
    assert!(
        store
            .claim(&worker, at(10), at(20), 0)
            .await
            .unwrap()
            .is_empty()
    );
    let claimed = store.claim(&worker, at(10), at(20), 3).await.unwrap();
    assert_eq!(
        claimed
            .iter()
            .map(|value| value.id.clone())
            .collect::<Vec<_>>(),
        vec![rows[3].id.clone(), rows[2].id.clone(), rows[0].id.clone()]
    );
    for value in &claimed {
        assert_eq!(value.status, Status::Running);
        assert_eq!(
            value.lease,
            Some(Lease {
                worker: worker.clone(),
                until: at(20),
            })
        );
        assert_eq!(&store.get(&value.id).await.unwrap(), value);
        assert_eq!(value.version, 0);
    }
    let remaining = store.claim(&worker, at(10), at(20), 100).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, rows[1].id);
    for value in rows.iter().skip(4) {
        assert_eq!(&store.get(&value.id).await.unwrap(), value);
    }
}

#[tokio::test]
async fn sweep_returns_a_running_activity_whose_lease_expired() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = schema.store().await;
    let mut running = instance(0);
    running.status = Status::Running;
    running.lease = Some(Lease {
        worker: WorkerId("dead".into()),
        until: at(9),
    });
    running.in_flight = Some(InFlight {
        key: TaskKey::new("/do/0/notify", ExecutionKey::empty()),
        attempt: 0,
    });
    let mut parked = instance(1);
    parked.status = Status::Waiting(WaitReason::Timer);
    parked.lease = Some(Lease {
        worker: WorkerId("dead".into()),
        until: at(9),
    });
    store.create(running.clone()).await.unwrap();
    store.create(parked.clone()).await.unwrap();

    assert_eq!(store.sweep(at(10)).await.unwrap(), vec![running.id.clone()]);
    assert_eq!(store.get(&running.id).await.unwrap(), running);
    assert_eq!(store.get(&parked.id).await.unwrap(), parked);
}

#[tokio::test]
async fn latest_step_returns_the_highest_attempt_for_one_key() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = Arc::new(schema.store().await);
    let engine = engine(&store);
    let first = instance(0);
    store.create(first.clone()).await.unwrap();
    let early = step(&first.id, "/do/9/early", 0, 10);
    let retry = step(&first.id, "/do/9/early", 1, 20);
    let other = step(&first.id, "/do/0/late", 3, 5);
    let mut commit = Commit::new(first.id.clone(), 0, Status::Running, at(30));
    commit.step_results = vec![retry.clone(), other, early];
    engine.commit(commit, None).await.unwrap();

    let key = TaskKey::new("/do/9/early", ExecutionKey::empty());
    assert_eq!(
        store.latest_step(&first.id, &key).await.unwrap(),
        Some(retry)
    );
    assert_eq!(
        store
            .latest_step(
                &first.id,
                &TaskKey::new("/do/missing", ExecutionKey::empty())
            )
            .await
            .unwrap(),
        None
    );
    let missing = InstanceId::from_string(Uuid::now_v7().to_string());
    assert_eq!(store.latest_step(&missing, &key).await.unwrap(), None);
}

#[tokio::test]
async fn sweep_is_read_only_and_only_lists_running_leases_strictly_before_the_cutoff() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = schema.store().await;
    let mut originals = Vec::new();
    for (status, until) in [
        (Status::Running, Some(9)),
        (Status::Running, Some(10)),
        (Status::Running, Some(11)),
        (Status::Running, None),
        (Status::Runnable, Some(9)),
        (Status::Waiting(WaitReason::Timer), Some(9)),
        (Status::Cancelled, Some(9)),
    ] {
        let mut value = instance(0);
        value.status = status;
        value.lease = until.map(|until| Lease {
            worker: WorkerId("old".into()),
            until: at(until),
        });
        store.create(value.clone()).await.unwrap();
        originals.push(value);
    }
    assert_eq!(
        store.sweep(at(10)).await.unwrap(),
        vec![originals[0].id.clone()]
    );
    for value in originals {
        assert_eq!(store.get(&value.id).await.unwrap(), value);
    }
}

#[tokio::test]
async fn a_commit_updates_all_fields_and_can_keep_then_release_the_lease() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = Arc::new(schema.store().await);
    let engine = engine(&store);
    let original = instance(0);
    store.create(original.clone()).await.unwrap();
    let claimed = store
        .claim(engine.worker(), at(0), at(30), 1)
        .await
        .unwrap()
        .remove(0);
    let mut commit = Commit::new(
        original.id.clone(),
        0,
        Status::Waiting(WaitReason::Timer),
        at(10),
    );
    commit.continuation = Some(vec![3, 1, 4]);
    commit.wake_at = Some(at(20));
    commit.output = Some(json!({"output": 42}));
    commit.fault = Some(tideloom_core::Fault::runtime("parent", "test"));
    commit.release_lease = false;
    let mut expected = claimed;
    expected.status = commit.status;
    expected.continuation = commit.continuation.clone();
    expected.wake_at = commit.wake_at;
    expected.output = commit.output.clone();
    expected.fault = commit.fault.clone();
    expected.version = 1;
    expected.updated_at = at(10);
    engine.commit(commit, None).await.unwrap();
    assert_eq!(store.get(&original.id).await.unwrap(), expected);
    let mut commit = Commit::new(original.id.clone(), 1, Status::Completed, at(25));
    commit.output = Some(json!({"done": true}));
    engine.commit(commit, None).await.unwrap();
    expected.status = Status::Completed;
    expected.continuation = None;
    expected.wake_at = None;
    expected.output = Some(json!({"done": true}));
    expected.fault = None;
    expected.lease = None;
    expected.version = 2;
    expected.updated_at = at(25);
    assert_eq!(store.get(&original.id).await.unwrap(), expected);
}

#[tokio::test]
async fn a_version_conflict_changes_neither_instances_nor_journal() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = Arc::new(schema.store().await);
    let engine = engine(&store);
    let parent = instance(0);
    let sibling = instance(1);
    store.create(parent.clone()).await.unwrap();
    store.create(sibling.clone()).await.unwrap();
    let row = step(&parent.id, "/do/0/http", 0, 5);
    let mut first = Commit::new(parent.id.clone(), 0, Status::Running, at(5));
    first.step_results.push(row.clone());
    engine.commit(first, None).await.unwrap();
    let before = store.get(&parent.id).await.unwrap();
    let mut stale = Commit::new(parent.id.clone(), 0, Status::Completed, at(10));
    stale.output = Some(json!("stale"));
    stale.continuation = Some(vec![9]);
    stale.wake_at = Some(at(99));
    stale.fault = Some(tideloom_core::Fault::runtime("parent", "stale"));
    stale.spawn_children.push(child(&parent, Some(0)));
    stale.cancel.push(sibling.id.clone());
    let mut overwritten = row.clone();
    overwritten.outcome = StepOutcome::Unknown;
    stale.step_results.push(overwritten);
    assert!(matches!(
        engine.commit(stale, None).await,
        Err(EngineError::Store(StoreError::VersionConflict {
            instance,
            expected: 0,
            found: 1
        })) if instance == parent.id
    ));
    assert_eq!(store.get(&parent.id).await.unwrap(), before);
    assert_eq!(store.get(&sibling.id).await.unwrap(), sibling);
    assert!(store.children(&parent.id).await.unwrap().is_empty());
    assert_eq!(store.steps(&parent.id).await.unwrap(), vec![row]);
}

#[tokio::test]
async fn spawning_children_lands_with_the_parent_and_duplicate_ids_roll_back_everything() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = Arc::new(schema.store().await);
    let engine = engine(&store);
    let parent = instance(0);
    store.create(parent.clone()).await.unwrap();
    let spawned = [child(&parent, Some(0)), child(&parent, Some(1))];
    let mut commit = Commit::new(
        parent.id.clone(),
        0,
        Status::Waiting(WaitReason::Join),
        at(12),
    );
    commit.spawn_children = spawned.to_vec();
    engine.commit(commit, None).await.unwrap();
    let before = store.get(&parent.id).await.unwrap();
    assert_eq!(before.status, Status::Waiting(WaitReason::Join));
    for (value, spec) in store
        .children(&parent.id)
        .await
        .unwrap()
        .iter()
        .zip(&spawned)
    {
        let mut expected = instance_with(Uuid::parse_str(spec.id.as_str()).unwrap(), 12);
        expected.parent = Some(spec.parent.clone());
        expected.input = spec.input.clone();
        assert_eq!(value, &expected);
    }
    let repeated = child(&parent, Some(2));
    for duplicate in [spawned[0].clone(), repeated.clone()] {
        // The second commit names one new id twice, the same way MemoryStore
        // rejects a batch that contains its own duplicate.
        let fresh = if duplicate.id == spawned[0].id {
            child(&parent, Some(2))
        } else {
            duplicate.clone()
        };
        let mut commit = Commit::new(parent.id.clone(), 1, Status::Completed, at(20));
        commit.spawn_children = vec![fresh.clone(), duplicate.clone()];
        commit.cancel.push(spawned[1].id.clone());
        commit
            .step_results
            .push(step(&parent.id, "/do/0/http", 0, 20));
        assert!(matches!(
            engine.commit(commit, None).await,
            Err(EngineError::Store(StoreError::AlreadyExists(id))) if id == duplicate.id
        ));
        assert_eq!(store.get(&parent.id).await.unwrap(), before);
        assert_eq!(store.children(&parent.id).await.unwrap().len(), 2);
        assert!(store.steps(&parent.id).await.unwrap().is_empty());
        assert!(matches!(
            store.get(&fresh.id).await,
            Err(StoreError::NotFound(_))
        ));
        assert_eq!(
            store.get(&spawned[1].id).await.unwrap().status,
            Status::Runnable
        );
    }
}

#[tokio::test]
async fn cancellation_skips_terminal_instances_and_invalidates_in_flight_commits() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = Arc::new(schema.store().await);
    let engine = engine(&store);
    let parent = instance(0);
    store.create(parent.clone()).await.unwrap();
    let mut originals = Vec::new();
    for status in [
        Status::Running,
        Status::Waiting(WaitReason::Join),
        Status::Completed,
        Status::Faulted,
        Status::Cancelled,
    ] {
        let mut value = instance(1);
        value.status = status;
        value.version = 7;
        value.continuation = (!status.is_terminal()).then(|| vec![8]);
        value.lease = Some(Lease {
            worker: WorkerId("old".into()),
            until: at(50),
        });
        store.create(value.clone()).await.unwrap();
        originals.push(value);
    }
    let mut commit = Commit::new(parent.id.clone(), 0, Status::Completed, at(10));
    commit.cancel = originals.iter().map(|value| value.id.clone()).collect();
    commit.cancel.push(originals[0].id.clone());
    engine.commit(commit, None).await.unwrap();
    for value in originals {
        let mut expected = value.clone();
        if !value.status.is_terminal() {
            expected.status = Status::Cancelled;
            expected.continuation = None;
            expected.wake_at = None;
            expected.lease = None;
            expected.version = 8;
            expected.updated_at = at(10);
        }
        assert_eq!(store.get(&value.id).await.unwrap(), expected);
        if !value.status.is_terminal() {
            let stale = Commit::new(value.id.clone(), 7, Status::Completed, at(15));
            assert!(matches!(
                engine.commit(stale, None).await,
                Err(EngineError::Store(StoreError::VersionConflict {
                    found: 8,
                    ..
                }))
            ));
        }
    }
}

#[tokio::test]
async fn a_missing_commit_or_cancel_target_fails_without_partial_writes() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = Arc::new(schema.store().await);
    let engine = engine(&store);
    let parent = instance(0);
    store.create(parent.clone()).await.unwrap();
    let missing = InstanceId::from_string(Uuid::now_v7().to_string());
    assert!(matches!(
        engine
            .commit(
                Commit::new(missing.clone(), 0, Status::Completed, at(1)),
                None
            )
            .await,
        Err(EngineError::Store(StoreError::NotFound(id))) if id == missing
    ));
    let mut commit = Commit::new(parent.id.clone(), 0, Status::Completed, at(10));
    commit.spawn_children.push(child(&parent, None));
    commit.cancel = vec![parent.id.clone(), missing.clone()];
    commit
        .step_results
        .push(step(&parent.id, "/do/0/http", 0, 10));
    assert!(matches!(
        engine.commit(commit, None).await,
        Err(EngineError::Store(StoreError::NotFound(id))) if id == missing
    ));
    assert_eq!(store.get(&parent.id).await.unwrap(), parent);
    assert!(store.children(&parent.id).await.unwrap().is_empty());
    assert!(store.steps(&parent.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn children_are_filtered_then_sorted_by_optional_branch_and_creation() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = schema.store().await;
    let parent = InstanceId::from_string(Uuid::now_v7().to_string());
    let other = InstanceId::from_string(Uuid::now_v7().to_string());
    let mut ids = Vec::new();
    for (branch, created, owner) in [
        (Some(1), 30, parent.clone()),
        (Some(2), 1, parent.clone()),
        (Some(1), 10, parent.clone()),
        (Some(0), 50, parent.clone()),
        (None, 60, parent.clone()),
        (Some(0), 0, other),
    ] {
        let mut value = instance(created);
        value.parent = Some(ParentRef { id: owner, branch });
        store.create(value.clone()).await.unwrap();
        ids.push(value.id);
    }
    store.create(instance(0)).await.unwrap();
    let children = store.children(&parent).await.unwrap();
    assert_eq!(
        children
            .iter()
            .map(|value| value.id.clone())
            .collect::<Vec<_>>(),
        vec![
            ids[4].clone(),
            ids[3].clone(),
            ids[2].clone(),
            ids[0].clone(),
            ids[1].clone()
        ]
    );
}

#[tokio::test]
async fn wake_flips_only_an_unarmed_events_wait_and_next_due_skips_other_waits() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = schema.store().await;
    let mut events = instance(10);
    events.status = Status::Waiting(WaitReason::Events);
    events.wake_at = None;
    events.version = 3;
    let mut armed = instance(11);
    armed.status = Status::Waiting(WaitReason::Events);
    armed.wake_at = Some(at(50));
    let mut timer = instance(12);
    timer.status = Status::Waiting(WaitReason::Timer);
    timer.wake_at = Some(at(80));
    let mut join = instance(13);
    join.status = Status::Waiting(WaitReason::Join);
    join.wake_at = Some(at(40));
    let mut running = instance(14);
    running.status = Status::Running;
    running.lease = Some(Lease {
        worker: WorkerId("holder".into()),
        until: at(200),
    });
    running.wake_at = Some(at(30));
    let mut runnable = instance(15);
    runnable.wake_at = Some(at(90));
    for row in [&events, &armed, &timer, &join, &running, &runnable] {
        store.create(row.clone()).await.unwrap();
    }

    let flipped = store
        .wake(
            &[
                events.id.clone(),
                armed.id.clone(),
                timer.id.clone(),
                InstanceId::from_string(Uuid::now_v7().to_string()),
            ],
            at(60),
        )
        .await
        .unwrap();
    assert_eq!(flipped, 1, "only the unarmed events wait flips");
    let woken = store.get(&events.id).await.unwrap();
    assert_eq!(woken.status, Status::Runnable);
    assert_eq!(woken.wake_at, Some(at(60)));
    assert_eq!(woken.version, 4);
    assert_eq!(store.get(&armed.id).await.unwrap().version, 0);
    assert_eq!(
        store.get(&timer.id).await.unwrap().status,
        Status::Waiting(WaitReason::Timer)
    );

    assert_eq!(store.wake(&[events.id.clone()], at(70)).await.unwrap(), 0);
    assert_eq!(store.get(&events.id).await.unwrap().wake_at, Some(at(60)));

    assert_eq!(store.next_due(at(60)).await.unwrap(), Some(at(80)));
    assert_eq!(store.next_due(at(80)).await.unwrap(), Some(at(90)));
    assert_eq!(store.next_due(at(90)).await.unwrap(), None);
}

#[tokio::test]
async fn journal_upserts_by_instance_key_and_attempt_and_reads_in_time_order() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = Arc::new(schema.store().await);
    let engine = engine(&store);
    let first = instance(0);
    let other = instance(0);
    store.create(first.clone()).await.unwrap();
    store.create(other.clone()).await.unwrap();
    let early = step(&first.id, "/do/9/early", 0, 10);
    let late = step(&first.id, "/do/0/late", 0, 30);
    let retry = step(&first.id, "/do/9/early", 1, 20);
    let unrelated = step(&other.id, "/do/9/early", 0, 5);
    let mut commit = Commit::new(first.id.clone(), 0, Status::Running, at(30));
    commit.step_results = vec![late.clone(), early.clone(), retry.clone()];
    engine.commit(commit, None).await.unwrap();
    let mut commit = Commit::new(other.id.clone(), 0, Status::Running, at(30));
    commit.step_results.push(unrelated.clone());
    engine.commit(commit, None).await.unwrap();
    let mut done = early;
    done.outcome = StepOutcome::Output(json!({"ok": true}));
    done.finished_at = at(15);
    let mut commit = Commit::new(first.id.clone(), 1, Status::Completed, at(40));
    commit.step_results.push(done.clone());
    engine.commit(commit, None).await.unwrap();
    assert_eq!(
        store.steps(&first.id).await.unwrap(),
        vec![done, retry, late]
    );
    assert_eq!(store.steps(&other.id).await.unwrap(), vec![unrelated]);
    let missing = InstanceId::from_string(Uuid::now_v7().to_string());
    assert!(store.steps(&missing).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_invalid_instance_id_is_a_backend_error() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let store = schema.store().await;
    let error = store
        .get(&InstanceId::from_string("not-a-uuid"))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Backend(_)));
}

pub struct UnusedBackends;

#[async_trait::async_trait]
impl Wakeup for UnusedBackends {
    async fn notify(
        &self,
        _: InstanceId,
        _: tideloom_runtime::Hint,
    ) -> Result<tideloom_runtime::Offset, tideloom_runtime::BusError> {
        panic!("unexpected notify")
    }
    async fn watch(
        &self,
        _: tideloom_runtime::Offset,
    ) -> Result<tideloom_runtime::SignalStream, tideloom_runtime::BusError> {
        panic!("unexpected watch")
    }
    async fn commit_offset(
        &self,
        _: tideloom_runtime::Offset,
    ) -> Result<(), tideloom_runtime::BusError> {
        panic!("unexpected offset commit")
    }
    async fn committed(&self) -> Result<tideloom_runtime::Offset, tideloom_runtime::BusError> {
        panic!("unexpected offset read")
    }
}

#[async_trait::async_trait]
impl EventSink for UnusedBackends {
    async fn publish(&self, _: CloudEvent) -> Result<Vec<InstanceId>, EventError> {
        panic!("unexpected event publish")
    }
}

#[async_trait::async_trait]
impl EventSource for UnusedBackends {
    async fn subscribe(&self, _: &InstanceId, _: ListenSpec) -> Result<(), EventError> {
        panic!("unexpected subscribe")
    }
    async fn unsubscribe(&self, _: &InstanceId) -> Result<(), EventError> {
        panic!("unexpected unsubscribe")
    }
    async fn inbox(&self, _: &InstanceId) -> Result<Vec<CloudEvent>, EventError> {
        panic!("unexpected inbox read")
    }
    async fn ack(&self, _: &InstanceId, _: &[String]) -> Result<(), EventError> {
        panic!("unexpected event ack")
    }
}
