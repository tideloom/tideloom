//! The in-memory store contract, with every write routed through Engine::commit.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::Barrier;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use async_trait::async_trait;
use serde_json::json;
use tideloom_core::ExecutionKey;
use tideloom_core::ListenSpec;
use tideloom_core::TaskKey;
use tideloom_core::Timestamp;
use tideloom_runtime::*;

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn instance(id: &str, created: u64) -> Instance {
    let mut instance = Instance::new(
        DefinitionKey {
            namespace: "test".into(),
            name: "memory".into(),
            version: "1".into(),
            content_hash: "pinned".into(),
        },
        None,
        json!({"input": id}),
        at(created),
    );
    instance.id = InstanceId::from_string(id);
    instance
}

// MemoryStore and these fixture backends never suspend; no async runtime is needed.
fn run<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    match pin!(future).as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("memory store unexpectedly suspended"),
    }
}

fn engine(store: &Arc<MemoryStore>) -> Engine {
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

fn child(parent: &Instance, id: &str, branch: Option<usize>) -> SpawnChild {
    SpawnChild {
        id: InstanceId::from_string(id),
        definition: parent.definition.clone(),
        parent: ParentRef {
            id: parent.id.clone(),
            branch,
        },
        input: json!({"child": id}),
        root_task: None,
    }
}

#[test]
fn creating_an_existing_id_does_not_replace_it_and_reads_are_snapshots() {
    let store = MemoryStore::new();
    let original = instance("original", 10);
    run(store.create(original.clone())).unwrap();
    let mut duplicate = original.clone();
    duplicate.input = json!("replacement");
    assert!(matches!(
        run(store.create(duplicate)),
        Err(StoreError::AlreadyExists(id)) if id == original.id
    ));
    let mut snapshot = run(store.get(&original.id)).unwrap();
    snapshot.continuation = Some(vec![1, 2]);
    assert_eq!(run(store.get(&original.id)).unwrap(), original);
    assert!(matches!(
        run(store.get(&InstanceId::from_string("missing"))),
        Err(StoreError::NotFound(_))
    ));
}

#[test]
fn claims_are_due_ordered_limited_and_exclude_live_leases_and_other_waits() {
    let store = MemoryStore::new();
    let worker = WorkerId("worker".into());
    let mut expected = Vec::new();
    for (id, created, status, wake, lease_until) in [
        ("late", 9, Status::Runnable, Some(10), None),
        (
            "timer",
            2,
            Status::Waiting(WaitReason::Timer),
            Some(10),
            None,
        ),
        ("expired", 4, Status::Runnable, Some(4), Some(10)),
        ("no-deadline", 7, Status::Runnable, None, None),
        ("future", 1, Status::Runnable, Some(11), None),
        (
            "future-timer",
            1,
            Status::Waiting(WaitReason::Timer),
            Some(11),
            None,
        ),
        (
            "undated-timer",
            1,
            Status::Waiting(WaitReason::Timer),
            None,
            None,
        ),
        ("leased", 1, Status::Runnable, Some(1), Some(11)),
        ("running", 1, Status::Running, Some(1), Some(9)),
        ("join", 1, Status::Waiting(WaitReason::Join), Some(1), None),
        (
            "child",
            1,
            Status::Waiting(WaitReason::Child),
            Some(1),
            None,
        ),
        (
            "events",
            1,
            Status::Waiting(WaitReason::Events),
            Some(1),
            None,
        ),
        ("completed", 1, Status::Completed, Some(1), None),
        ("faulted", 1, Status::Faulted, Some(1), None),
        ("cancelled", 1, Status::Cancelled, Some(1), None),
    ] {
        let mut value = instance(id, created);
        value.status = status;
        value.wake_at = wake.map(at);
        value.lease = lease_until.map(|until| Lease {
            worker: WorkerId("old".into()),
            until: at(until),
        });
        run(store.create(value.clone())).unwrap();
        expected.push(value);
    }
    assert!(
        run(store.claim(&worker, at(10), at(20), 0))
            .unwrap()
            .is_empty()
    );
    let claimed = run(store.claim(&worker, at(10), at(20), 3)).unwrap();
    assert_eq!(
        claimed
            .iter()
            .map(|value| value.id.as_str())
            .collect::<Vec<_>>(),
        ["no-deadline", "expired", "late"]
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
        assert_eq!(&run(store.get(&value.id)).unwrap(), value);
        assert_eq!(value.version, 0);
    }
    let remaining = run(store.claim(&worker, at(10), at(20), 100)).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id.as_str(), "timer");
    for value in expected.iter().skip(4) {
        assert_eq!(&run(store.get(&value.id)).unwrap(), value);
    }
}

#[test]
fn two_workers_claiming_together_only_get_one_instance_between_them() {
    let store = Arc::new(MemoryStore::new());
    let original = instance("exclusive", 0);
    run(store.create(original.clone())).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|name| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                run(store.claim(&WorkerId(name.into()), at(0), at(10), 1)).unwrap()
            })
        })
        .collect();
    let claimed: Vec<_> = workers
        .into_iter()
        .flat_map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, original.id);
    assert_eq!(run(store.get(&original.id)).unwrap(), claimed[0]);
}

#[test]
fn sweep_returns_a_running_activity_whose_lease_expired() {
    let store = MemoryStore::new();
    let mut running = instance("in-flight", 0);
    running.status = Status::Running;
    running.lease = Some(Lease {
        worker: WorkerId("dead".into()),
        until: at(9),
    });
    running.in_flight = Some(InFlight {
        key: TaskKey::new("/do/0/notify", ExecutionKey::empty()),
        attempt: 0,
    });
    let mut parked = instance("waiting-activity-is-not-a-status", 1);
    parked.status = Status::Waiting(WaitReason::Timer);
    parked.lease = Some(Lease {
        worker: WorkerId("dead".into()),
        until: at(9),
    });
    run(store.create(running.clone())).unwrap();
    run(store.create(parked.clone())).unwrap();

    assert_eq!(
        run(store.sweep(at(10))).unwrap(),
        vec![InstanceId::from_string("in-flight")]
    );
    assert_eq!(run(store.get(&running.id)).unwrap(), running);
    assert_eq!(run(store.get(&parked.id)).unwrap(), parked);
}

#[test]
fn latest_step_returns_the_highest_attempt_for_one_key() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(&store);
    let first = instance("first", 0);
    run(store.create(first.clone())).unwrap();
    let early = step(&first.id, "/do/9/early", 0, 10);
    let retry = step(&first.id, "/do/9/early", 1, 20);
    let other = step(&first.id, "/do/0/late", 3, 5);
    let mut commit = Commit::new(first.id.clone(), 0, Status::Running, at(30));
    commit.step_results = vec![retry.clone(), other.clone(), early];
    run(engine.commit(commit, None)).unwrap();

    let key = TaskKey::new("/do/9/early", ExecutionKey::empty());
    assert_eq!(
        run(store.latest_step(&first.id, &key)).unwrap(),
        Some(retry)
    );
    assert_eq!(
        run(store.latest_step(
            &first.id,
            &TaskKey::new("/do/missing", ExecutionKey::empty())
        ))
        .unwrap(),
        None
    );
    assert_eq!(
        run(store.latest_step(&InstanceId::from_string("missing"), &key)).unwrap(),
        None
    );
}

#[test]
fn sweep_is_read_only_and_only_lists_running_leases_strictly_before_the_cutoff() {
    let store = MemoryStore::new();
    let mut originals = Vec::new();
    for (id, status, until) in [
        ("expired", Status::Running, Some(9)),
        ("boundary", Status::Running, Some(10)),
        ("live", Status::Running, Some(11)),
        ("unleased", Status::Running, None),
        ("runnable", Status::Runnable, Some(9)),
        ("waiting", Status::Waiting(WaitReason::Timer), Some(9)),
        ("terminal", Status::Cancelled, Some(9)),
    ] {
        let mut value = instance(id, 0);
        value.status = status;
        value.lease = until.map(|until| Lease {
            worker: WorkerId("old".into()),
            until: at(until),
        });
        run(store.create(value.clone())).unwrap();
        originals.push(value);
    }
    assert_eq!(
        run(store.sweep(at(10))).unwrap(),
        vec![InstanceId::from_string("expired")]
    );
    for value in originals {
        assert_eq!(run(store.get(&value.id)).unwrap(), value);
    }
}

#[test]
fn a_commit_updates_all_fields_and_can_keep_then_release_the_lease() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(&store);
    let original = instance("parent", 0);
    run(store.create(original.clone())).unwrap();
    let claimed = run(store.claim(engine.worker(), at(0), at(30), 1))
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
    run(engine.commit(commit, None)).unwrap();
    assert_eq!(run(store.get(&original.id)).unwrap(), expected);
    let mut commit = Commit::new(original.id.clone(), 1, Status::Completed, at(25));
    commit.output = Some(json!({"done": true}));
    run(engine.commit(commit, None)).unwrap();
    expected.status = Status::Completed;
    expected.continuation = None;
    expected.wake_at = None;
    expected.output = Some(json!({"done": true}));
    expected.fault = None;
    expected.lease = None;
    expected.version = 2;
    expected.updated_at = at(25);
    assert_eq!(run(store.get(&original.id)).unwrap(), expected);
}

#[test]
fn a_version_conflict_changes_neither_instances_nor_journal() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(&store);
    let parent = instance("parent", 0);
    let sibling = instance("sibling", 1);
    run(store.create(parent.clone())).unwrap();
    run(store.create(sibling.clone())).unwrap();
    let row = step(&parent.id, "/do/0/http", 0, 5);
    let mut first = Commit::new(parent.id.clone(), 0, Status::Running, at(5));
    first.step_results.push(row.clone());
    run(engine.commit(first, None)).unwrap();
    let before = run(store.get(&parent.id)).unwrap();
    let mut stale = Commit::new(parent.id.clone(), 0, Status::Completed, at(10));
    stale.output = Some(json!("stale"));
    stale.continuation = Some(vec![9]);
    stale.wake_at = Some(at(99));
    stale.fault = Some(tideloom_core::Fault::runtime("parent", "stale"));
    stale
        .spawn_children
        .push(child(&parent, "new-child", Some(0)));
    stale.cancel.push(sibling.id.clone());
    let mut overwritten = row.clone();
    overwritten.outcome = StepOutcome::Unknown;
    stale.step_results.push(overwritten);
    assert!(
        matches!(run(engine.commit(stale, None)), Err(EngineError::Store(
        StoreError::VersionConflict { instance, expected: 0, found: 1 }
    )) if instance == parent.id)
    );
    assert_eq!(run(store.get(&parent.id)).unwrap(), before);
    assert_eq!(run(store.get(&sibling.id)).unwrap(), sibling);
    assert!(run(store.children(&parent.id)).unwrap().is_empty());
    assert_eq!(run(store.steps(&parent.id)).unwrap(), vec![row]);
}

#[test]
fn spawning_children_lands_with_the_parent_and_duplicate_ids_roll_back_everything() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(&store);
    let parent = instance("parent", 0);
    run(store.create(parent.clone())).unwrap();
    let spawned = [
        child(&parent, "left", Some(0)),
        child(&parent, "right", Some(1)),
    ];
    let mut commit = Commit::new(
        parent.id.clone(),
        0,
        Status::Waiting(WaitReason::Join),
        at(12),
    );
    commit.spawn_children = spawned.to_vec();
    run(engine.commit(commit, None)).unwrap();
    let before = run(store.get(&parent.id)).unwrap();
    assert_eq!(before.status, Status::Waiting(WaitReason::Join));
    for (value, spec) in run(store.children(&parent.id))
        .unwrap()
        .iter()
        .zip(&spawned)
    {
        let mut expected = instance(spec.id.as_str(), 12);
        expected.parent = Some(spec.parent.clone());
        expected.input = spec.input.clone();
        assert_eq!(value, &expected);
    }
    for duplicate in [spawned[0].clone(), child(&parent, "fresh", Some(2))] {
        let mut commit = Commit::new(parent.id.clone(), 1, Status::Completed, at(20));
        commit.spawn_children = vec![child(&parent, "fresh", Some(2)), duplicate.clone()];
        commit.cancel.push(spawned[1].id.clone());
        commit
            .step_results
            .push(step(&parent.id, "/do/0/http", 0, 20));
        assert!(
            matches!(run(engine.commit(commit, None)), Err(EngineError::Store(StoreError::AlreadyExists(id))) if id == duplicate.id)
        );
        assert_eq!(run(store.get(&parent.id)).unwrap(), before);
        assert_eq!(run(store.children(&parent.id)).unwrap().len(), 2);
        assert!(run(store.steps(&parent.id)).unwrap().is_empty());
        assert!(matches!(
            run(store.get(&InstanceId::from_string("fresh"))),
            Err(StoreError::NotFound(_))
        ));
        assert_eq!(
            run(store.get(&spawned[1].id)).unwrap().status,
            Status::Runnable
        );
    }
}

#[test]
fn cancellation_skips_terminal_instances_and_invalidates_in_flight_commits() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(&store);
    let parent = instance("parent", 0);
    run(store.create(parent.clone())).unwrap();
    let mut originals = Vec::new();
    for (id, status) in [
        ("running", Status::Running),
        ("waiting", Status::Waiting(WaitReason::Join)),
        ("completed", Status::Completed),
        ("faulted", Status::Faulted),
        ("cancelled", Status::Cancelled),
    ] {
        let mut value = instance(id, 1);
        value.status = status;
        value.version = 7;
        value.continuation = (!status.is_terminal()).then(|| vec![8]);
        value.lease = Some(Lease {
            worker: WorkerId("old".into()),
            until: at(50),
        });
        run(store.create(value.clone())).unwrap();
        originals.push(value);
    }
    let mut commit = Commit::new(parent.id.clone(), 0, Status::Completed, at(10));
    commit.cancel = originals.iter().map(|value| value.id.clone()).collect();
    commit.cancel.push(originals[0].id.clone());
    run(engine.commit(commit, None)).unwrap();
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
        assert_eq!(run(store.get(&value.id)).unwrap(), expected);
        if !value.status.is_terminal() {
            let stale = Commit::new(value.id.clone(), 7, Status::Completed, at(15));
            assert!(matches!(
                run(engine.commit(stale, None)),
                Err(EngineError::Store(StoreError::VersionConflict {
                    found: 8,
                    ..
                }))
            ));
        }
    }
}

#[test]
fn a_missing_commit_or_cancel_target_fails_without_partial_writes() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(&store);
    let parent = instance("parent", 0);
    run(store.create(parent.clone())).unwrap();
    let missing = InstanceId::from_string("missing");
    assert!(
        matches!(run(engine.commit(Commit::new(missing.clone(), 0, Status::Completed, at(1)), None)), Err(EngineError::Store(StoreError::NotFound(id))) if id == missing)
    );
    let mut commit = Commit::new(parent.id.clone(), 0, Status::Completed, at(10));
    commit
        .spawn_children
        .push(child(&parent, "new-child", None));
    commit.cancel = vec![parent.id.clone(), missing.clone()];
    commit
        .step_results
        .push(step(&parent.id, "/do/0/http", 0, 10));
    assert!(
        matches!(run(engine.commit(commit, None)), Err(EngineError::Store(StoreError::NotFound(id))) if id == missing)
    );
    assert_eq!(run(store.get(&parent.id)).unwrap(), parent);
    assert!(run(store.children(&parent.id)).unwrap().is_empty());
    assert!(run(store.steps(&parent.id)).unwrap().is_empty());
}

#[test]
fn children_are_filtered_then_sorted_by_optional_branch_and_creation() {
    let store = MemoryStore::new();
    let parent = InstanceId::from_string("parent");
    for (id, branch, created, owner) in [
        ("one-new", Some(1), 30, "parent"),
        ("two", Some(2), 1, "parent"),
        ("one-old", Some(1), 10, "parent"),
        ("zero", Some(0), 50, "parent"),
        ("workflow", None, 60, "parent"),
        ("unrelated", Some(0), 0, "other"),
    ] {
        let mut value = instance(id, created);
        value.parent = Some(ParentRef {
            id: InstanceId::from_string(owner),
            branch,
        });
        run(store.create(value)).unwrap();
    }
    run(store.create(instance("root", 0))).unwrap();
    let children = run(store.children(&parent)).unwrap();
    assert_eq!(
        children
            .iter()
            .map(|value| value.id.as_str())
            .collect::<Vec<_>>(),
        ["workflow", "zero", "one-old", "one-new", "two"]
    );
}

#[test]
fn wake_flips_only_an_unarmed_events_wait_and_next_due_skips_other_waits() {
    let store = Arc::new(MemoryStore::new());
    let mut events = instance("events", 10);
    events.status = Status::Waiting(WaitReason::Events);
    events.wake_at = None;
    events.version = 3;
    let mut armed = instance("armed", 11);
    armed.status = Status::Waiting(WaitReason::Events);
    armed.wake_at = Some(at(50));
    let mut timer = instance("timer", 12);
    timer.status = Status::Waiting(WaitReason::Timer);
    timer.wake_at = Some(at(80));
    let mut join = instance("join", 13);
    join.status = Status::Waiting(WaitReason::Join);
    join.wake_at = Some(at(40));
    let mut running = instance("running", 14);
    running.status = Status::Running;
    running.lease = Some(Lease {
        worker: WorkerId("holder".into()),
        until: at(200),
    });
    running.wake_at = Some(at(30));
    let mut runnable = instance("runnable", 15);
    runnable.wake_at = Some(at(90));
    for row in [&events, &armed, &timer, &join, &running, &runnable] {
        run(store.create(row.clone())).unwrap();
    }

    let flipped = run(store.wake(
        &[
            events.id.clone(),
            armed.id.clone(),
            timer.id.clone(),
            InstanceId::from_string("missing"),
        ],
        at(60),
    ))
    .unwrap();
    assert_eq!(flipped, 1, "only the unarmed events wait flips");
    let woken = run(store.get(&events.id)).unwrap();
    assert_eq!(woken.status, Status::Runnable);
    assert_eq!(woken.wake_at, Some(at(60)));
    assert_eq!(woken.version, 4);
    assert_eq!(run(store.get(&armed.id)).unwrap().version, 0);
    assert_eq!(
        run(store.get(&timer.id)).unwrap().status,
        Status::Waiting(WaitReason::Timer)
    );

    // A second wake of the same id is a no-op: it already has a wake_at.
    assert_eq!(run(store.wake(&[events.id.clone()], at(70))).unwrap(), 0);
    assert_eq!(run(store.get(&events.id)).unwrap().wake_at, Some(at(60)));

    // 60 is not strictly after 60. The join is not claimable. The running
    // row's earlier wake_at is behind a live lease. The timer at 80 wins.
    assert_eq!(run(store.next_due(at(60))).unwrap(), Some(at(80)));
    assert_eq!(run(store.next_due(at(80))).unwrap(), Some(at(90)));
    assert_eq!(run(store.next_due(at(90))).unwrap(), None);
}

#[test]
fn journal_upserts_by_instance_key_and_attempt_and_reads_in_time_order() {
    let store = Arc::new(MemoryStore::new());
    let engine = engine(&store);
    let first = instance("first", 0);
    let other = instance("other", 0);
    run(store.create(first.clone())).unwrap();
    run(store.create(other.clone())).unwrap();
    let early = step(&first.id, "/do/9/early", 0, 10);
    let late = step(&first.id, "/do/0/late", 0, 30);
    let retry = step(&first.id, "/do/9/early", 1, 20);
    let unrelated = step(&other.id, "/do/9/early", 0, 5);
    let mut commit = Commit::new(first.id.clone(), 0, Status::Running, at(30));
    commit.step_results = vec![late.clone(), early.clone(), retry.clone()];
    run(engine.commit(commit, None)).unwrap();
    let mut commit = Commit::new(other.id.clone(), 0, Status::Running, at(30));
    commit.step_results.push(unrelated.clone());
    run(engine.commit(commit, None)).unwrap();
    let mut done = early;
    done.outcome = StepOutcome::Output(json!({"ok": true}));
    done.finished_at = at(15);
    let mut commit = Commit::new(first.id.clone(), 1, Status::Completed, at(40));
    commit.step_results.push(done.clone());
    run(engine.commit(commit, None)).unwrap();
    assert_eq!(
        run(store.steps(&first.id)).unwrap(),
        vec![done, retry, late]
    );
    assert_eq!(run(store.steps(&other.id)).unwrap(), vec![unrelated]);
    assert!(
        run(store.steps(&InstanceId::from_string("missing")))
            .unwrap()
            .is_empty()
    );
}

// These services are not involved in store commits without a signal offset.
struct UnusedBackends;

#[async_trait]
impl Wakeup for UnusedBackends {
    async fn notify(&self, _: InstanceId, _: Hint) -> Result<Offset, BusError> {
        panic!("unexpected notify")
    }
    async fn watch(&self, _: Offset) -> Result<SignalStream, BusError> {
        panic!("unexpected watch")
    }
    async fn commit_offset(&self, _: Offset) -> Result<(), BusError> {
        panic!("unexpected offset commit")
    }
    async fn committed(&self) -> Result<Offset, BusError> {
        panic!("unexpected offset read")
    }
}

#[async_trait]
impl EventSink for UnusedBackends {
    async fn publish(&self, _: CloudEvent) -> Result<Vec<InstanceId>, EventError> {
        panic!("unexpected event publish")
    }
}

#[async_trait]
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
