//! PgStore against a real PostgreSQL, plus the two guarantees MemoryStore
//! cannot show: concurrent claims on two connections do not overlap, and a
//! failed spawn rolls the whole apply back.

mod store_contract;

use std::sync::Arc;

use tideloom_core::Timestamp;
use tideloom_runtime::Commit;
use tideloom_runtime::Engine;
use tideloom_runtime::EngineError;
use tideloom_runtime::EngineParts;
use tideloom_runtime::InstanceId;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::ManualClock;
use tideloom_runtime::MemoryDefinitions;
use tideloom_runtime::ParentRef;
use tideloom_runtime::SpawnChild;
use tideloom_runtime::Status;
use tideloom_runtime::StoreError;
use tideloom_runtime::WorkerId;
use uuid::Uuid;

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn skip_without_database() -> bool {
    if std::env::var_os("TIDELOOM_TEST_DATABASE_URL").is_none() {
        eprintln!("skipping PgStore tests: TIDELOOM_TEST_DATABASE_URL is unset");
        return true;
    }
    false
}

#[tokio::test]
async fn two_connections_claiming_the_same_batch_do_not_overlap() {
    if skip_without_database() {
        return;
    }
    let schema = store_contract::schema().await;
    let left = Arc::new(schema.store().await);
    let right = Arc::new(schema.store().await);
    let mut ids = Vec::new();
    for _ in 0..8 {
        // Every row is due at the claim instant. A later created_at would
        // not be claimable yet, which is a different test.
        let row = store_contract::bare_instance(0);
        ids.push(row.id.clone());
        left.create(row).await.unwrap();
    }
    let left_worker = WorkerId("left".into());
    let right_worker = WorkerId("right".into());
    // Start both claims, then let them run together. A barrier inside the
    // store is not available; holding both futures until both tasks are
    // spawned is what makes SKIP LOCKED observable.
    let (left_claimed, right_claimed) = tokio::join!(
        left.claim(&left_worker, at(0), at(50), 8),
        right.claim(&right_worker, at(0), at(50), 8),
    );
    let left_claimed = left_claimed.unwrap();
    let right_claimed = right_claimed.unwrap();
    assert_eq!(
        left_claimed.len() + right_claimed.len(),
        8,
        "left {} right {}",
        left_claimed.len(),
        right_claimed.len()
    );
    let mut seen = left_claimed
        .iter()
        .chain(&right_claimed)
        .map(|row| row.id.clone())
        .collect::<Vec<_>>();
    seen.sort();
    let mut expected = ids;
    expected.sort();
    assert_eq!(seen, expected);
    for row in left_claimed {
        assert_eq!(
            row.lease.as_ref().map(|lease| &lease.worker),
            Some(&left_worker)
        );
        assert_eq!(left.get(&row.id).await.unwrap().lease, row.lease);
    }
    for row in right_claimed {
        assert_eq!(
            row.lease.as_ref().map(|lease| &lease.worker),
            Some(&right_worker)
        );
    }
}

#[tokio::test]
async fn a_duplicate_spawn_in_the_middle_of_apply_leaves_no_partial_write() {
    if skip_without_database() {
        return;
    }
    let schema = store_contract::schema().await;
    let store = Arc::new(schema.store().await);
    let engine = Engine::new(EngineParts {
        worker: WorkerId("worker".into()),
        store: store.clone(),
        wakeup: Arc::new(store_contract::UnusedBackends),
        definitions: Arc::new(MemoryDefinitions::new()),
        executors: Vec::new(),
        events_out: Arc::new(store_contract::UnusedBackends),
        events_in: Arc::new(store_contract::UnusedBackends),
        clock: Arc::new(ManualClock::at(at(0))),
    });
    let parent = store_contract::bare_instance(0);
    store.create(parent.clone()).await.unwrap();
    let existing = store_contract::bare_instance(1);
    store.create(existing.clone()).await.unwrap();
    let fresh = InstanceId::from_string(Uuid::now_v7().to_string());
    let mut commit = Commit::new(
        parent.id.clone(),
        0,
        Status::Waiting(tideloom_runtime::WaitReason::Join),
        at(12),
    );
    commit.spawn_children = vec![
        SpawnChild {
            id: fresh.clone(),
            definition: parent.definition.clone(),
            parent: ParentRef {
                id: parent.id.clone(),
                branch: Some(0),
            },
            input: serde_json::json!({"child": "fresh"}),
            root_task: Some("/do/0".into()),
        },
        SpawnChild {
            id: existing.id.clone(),
            definition: parent.definition.clone(),
            parent: ParentRef {
                id: parent.id.clone(),
                branch: Some(1),
            },
            input: serde_json::json!({"child": "duplicate"}),
            root_task: None,
        },
    ];
    assert!(matches!(
        engine.commit(commit, None).await,
        Err(EngineError::Store(StoreError::AlreadyExists(id))) if id == existing.id
    ));
    assert_eq!(store.get(&parent.id).await.unwrap(), parent);
    assert!(matches!(
        store.get(&fresh).await,
        Err(StoreError::NotFound(_))
    ));
    assert_eq!(store.get(&existing.id).await.unwrap(), existing);
    assert!(store.children(&parent.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_raised_fault_and_an_at_sign_in_a_position_round_trip() {
    if skip_without_database() {
        return;
    }
    let schema = store_contract::schema().await;
    let store = schema.store().await;
    let mut row = store_contract::bare_instance(0);
    // Not a constructor fault: type, status, and title are the document's.
    let fault = serde_json::from_value(serde_json::json!({
        "error_type": "https://example.test/errors/raised",
        "status": 422,
        "instance": "/do/0",
        "title": "raised",
        "detail": "from the document"
    }))
    .unwrap();
    row.fault = Some(fault);
    row.in_flight = Some(tideloom_runtime::InFlight {
        key: tideloom_core::TaskKey::new(
            "/do/0/call@partner",
            tideloom_core::ExecutionKey::new([tideloom_core::Counter::Loop(2)]),
        ),
        attempt: 1,
    });
    store.create(row.clone()).await.unwrap();
    assert_eq!(store.get(&row.id).await.unwrap(), row);
}
