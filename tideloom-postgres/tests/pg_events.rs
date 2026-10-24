//! PgEventBus against a real PostgreSQL.
//!
//! An event published before subscribe is dropped. A repeated publish of the
//! same event id does not land again. Replacing a spec keeps the inbox.
//! `connect_waking` flips only an unarmed events wait, and a later
//! `InstanceStore::wake` of the same row matches nothing.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::json;
use sqlx::types::time::OffsetDateTime;
use tideloom_core::EventFilter;
use tideloom_core::ListenSpec;
use tideloom_core::ListenStrategy;
use tideloom_core::Timestamp;
use tideloom_postgres::PgEventBus;
use tideloom_postgres::PgStore;
use tideloom_runtime::CloudEvent;
use tideloom_runtime::EventSink;
use tideloom_runtime::EventSource;
use tideloom_runtime::Instance;
use tideloom_runtime::InstanceId;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::ManualClock;
use tideloom_runtime::Status;
use tideloom_runtime::WaitReason;
use uuid::Uuid;

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn database_available() -> bool {
    if std::env::var_os("TIDELOOM_TEST_DATABASE_URL").is_none() {
        eprintln!("skipping PgEventBus tests: TIDELOOM_TEST_DATABASE_URL is unset");
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
        let name = format!("s3_{}", Uuid::now_v7().simple());
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

    async fn bus(&self) -> PgEventBus {
        PgEventBus::connect(&self.url).await.expect("connect bus")
    }

    async fn waking_bus(&self) -> PgEventBus {
        PgEventBus::connect_waking(&self.url)
            .await
            .expect("connect waking bus")
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

fn instance_id() -> InstanceId {
    InstanceId::from_string(Uuid::now_v7().to_string())
}

fn one(pairs: &[(&str, &str)]) -> ListenSpec {
    ListenSpec {
        strategy: ListenStrategy::One,
        filters: vec![filter(pairs)],
        read: "data".to_string(),
    }
}

fn spec(strategy: ListenStrategy, filters: Vec<EventFilter>) -> ListenSpec {
    ListenSpec {
        strategy,
        filters,
        read: "data".to_string(),
    }
}

fn filter(pairs: &[(&str, &str)]) -> EventFilter {
    EventFilter {
        attributes: pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect(),
    }
}

fn event(id: &str, type_name: &str, data: serde_json::Value) -> CloudEvent {
    CloudEvent {
        id: id.to_string(),
        r#type: type_name.to_string(),
        source: "https://example.test/orders".to_string(),
        subject: None,
        time: None,
        extensions: BTreeMap::new(),
        data,
    }
}

fn waiting_instance(id: InstanceId, status: Status, wake_at: Option<Timestamp>) -> Instance {
    Instance {
        id,
        definition: tideloom_runtime::DefinitionKey {
            namespace: "test".into(),
            name: "events".into(),
            version: "1".into(),
            content_hash: "hash".into(),
        },
        parent: None,
        root_task: None,
        status,
        version: 0,
        lease: None,
        in_flight: None,
        wake_at,
        continuation: None,
        input: json!({}),
        output: None,
        fault: None,
        created_at: at(1),
        updated_at: at(1),
    }
}

#[tokio::test]
async fn an_event_published_before_subscribe_is_lost() {
    if !database_available() {
        return;
    }
    let database = Database::create().await;
    let bus = database.bus().await;
    let listener = instance_id();
    let landed = bus
        .publish(event(
            "early-paid",
            "io.example.paid",
            json!({"early": true}),
        ))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        landed.is_empty(),
        "nobody was subscribed, so the event lands nowhere"
    );
    bus.subscribe(&listener, one(&[("type", "io.example.paid")]))
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    let inbox = bus
        .inbox(&listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        inbox.is_empty(),
        "subscribe does not replay an event published before it"
    );
}

#[tokio::test]
async fn publish_lands_only_a_full_filter_match_and_a_repeat_returns_nothing() {
    if !database_available() {
        return;
    }
    let database = Database::create().await;
    let bus = database.bus().await;
    let listener = instance_id();
    let other = instance_id();
    // Two attributes: sharing only `type` is not a match. The other listener
    // shares nothing with a paid event, so the candidate query must not return it.
    bus.subscribe_at(
        &listener,
        one(&[
            ("type", "io.example.paid"),
            ("source", "https://example.test/orders"),
        ]),
        at(10),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    bus.subscribe_at(&other, one(&[("type", "io.example.other")]), at(10))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let before_repeat: sqlx::types::time::OffsetDateTime =
        sqlx::query_scalar("SELECT updated_at FROM event_subscription WHERE instance_id = $1")
            .bind(sqlx::types::Uuid::parse_str(listener.as_str()).unwrap())
            .fetch_one(bus.pool())
            .await
            .unwrap();
    bus.subscribe(
        &listener,
        one(&[
            ("type", "io.example.paid"),
            ("source", "https://example.test/orders"),
        ]),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    let after_repeat: sqlx::types::time::OffsetDateTime =
        sqlx::query_scalar("SELECT updated_at FROM event_subscription WHERE instance_id = $1")
            .bind(sqlx::types::Uuid::parse_str(listener.as_str()).unwrap())
            .fetch_one(bus.pool())
            .await
            .unwrap();
    assert_eq!(
        before_repeat, after_repeat,
        "the same spec does not rewrite the subscription"
    );

    let mut partial = event("partial", "io.example.paid", json!({"no": true}));
    partial.source = "https://example.test/elsewhere".into();
    let landed = bus
        .publish_at(partial, at(12))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        landed.is_empty(),
        "one shared attribute is not a filter match"
    );
    assert!(
        bus.inbox(&listener)
            .await
            .unwrap_or_else(|error| panic!("{error}"))
            .is_empty()
    );

    let paid = event("paid-1", "io.example.paid", json!({"order": 7}));
    let landed = bus
        .publish_at(paid.clone(), at(30))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(landed, vec![listener.clone()]);
    let again = bus
        .publish_at(paid, at(40))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(again.is_empty(), "a repeated event id is not a new landing");
    let inbox = bus
        .inbox(&listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].id, "paid-1");
    assert_eq!(inbox[0].data, json!({"order": 7}));
    assert!(
        bus.inbox(&other)
            .await
            .unwrap_or_else(|error| panic!("{error}"))
            .is_empty()
    );
}

#[tokio::test]
async fn any_filter_matches_and_inbox_keeps_arrival_order() {
    if !database_available() {
        return;
    }
    let database = Database::create().await;
    let bus = database.bus().await;
    let listener = instance_id();
    bus.subscribe_at(
        &listener,
        spec(
            ListenStrategy::Any,
            vec![
                filter(&[("type", "io.example.paid")]),
                filter(&[
                    ("type", "io.example.shipped"),
                    ("subject", "order-9"),
                    ("tenant", "acme"),
                ]),
            ],
        ),
        at(10),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));

    let mut shipped = event("shipped", "io.example.shipped", json!({"step": "shipped"}));
    shipped.subject = Some("order-9".into());
    shipped.extensions.insert("tenant".into(), "acme".into());
    let mut unmatched = event("no-subject", "io.example.shipped", json!({"step": "no"}));
    unmatched.subject = Some("order-other".into());
    for (when, item) in [
        (
            20,
            event("paid", "io.example.paid", json!({"step": "paid"})),
        ),
        (21, unmatched),
        (22, shipped),
    ] {
        bus.publish_at(item, at(when))
            .await
            .unwrap_or_else(|error| panic!("{error}"));
    }
    let inbox = bus
        .inbox(&listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        inbox
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        vec!["paid", "shipped"]
    );
}

#[tokio::test]
async fn replacing_a_spec_keeps_the_inbox_and_unsubscribe_drops_it() {
    if !database_available() {
        return;
    }
    let database = Database::create().await;
    let bus = database.bus().await;
    let listener = instance_id();
    bus.subscribe_at(&listener, one(&[("type", "io.example.paid")]), at(10))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    bus.publish_at(
        event("paid-1", "io.example.paid", json!({"order": 7})),
        at(20),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));

    bus.subscribe_at(&listener, one(&[("type", "io.example.shipped")]), at(30))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let kept = bus
        .inbox(&listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        kept.len(),
        1,
        "a new spec does not drop events already collected"
    );
    // The old filter is gone. A paid event no longer lands; a shipped one does.
    let landed = bus
        .publish_at(
            event("paid-2", "io.example.paid", json!({"order": 8})),
            at(40),
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(landed.is_empty());
    let landed = bus
        .publish_at(
            event("shipped-1", "io.example.shipped", json!({"order": 8})),
            at(41),
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(landed, vec![listener.clone()]);

    bus.ack(&listener, &[])
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    bus.ack(&listener, &["paid-1".to_string(), "missing".to_string()])
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let left = bus
        .inbox(&listener)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, "shipped-1");

    bus.unsubscribe_at(&listener, at(50))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        bus.inbox(&listener)
            .await
            .unwrap_or_else(|error| panic!("{error}"))
            .is_empty(),
        "unsubscribe deletes the inbox"
    );
    let landed = bus
        .publish_at(
            event("shipped-2", "io.example.shipped", json!({"order": 9})),
            at(60),
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        landed.is_empty(),
        "an inactive subscription does not collect"
    );

    // The inactive row still has a shipped attribute. A live listener on the
    // same value must still be found; the inactive one must not come back.
    let live = instance_id();
    bus.subscribe_at(&live, one(&[("type", "io.example.shipped")]), at(70))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let landed = bus
        .publish_at(
            event("shipped-3", "io.example.shipped", json!({"order": 10})),
            at(80),
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(landed, vec![live]);
    assert!(
        bus.inbox(&listener)
            .await
            .unwrap_or_else(|error| panic!("{error}"))
            .is_empty()
    );
}

#[tokio::test]
async fn resubscribe_after_unsubscribe_does_not_restore_the_old_inbox() {
    if !database_available() {
        return;
    }
    let database = Database::create().await;
    let bus = database.bus().await;
    let listener = instance_id();
    bus.subscribe_at(&listener, one(&[("type", "io.example.paid")]), at(10))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    bus.publish_at(
        event("paid-1", "io.example.paid", json!({"order": 1})),
        at(20),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    bus.unsubscribe_at(&listener, at(30))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    // Published while inactive. The unique instance row is reactivated, but
    // this event was not delivered to a live subscription, so it stays lost.
    bus.publish_at(
        event("paid-2", "io.example.paid", json!({"order": 2})),
        at(40),
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"));
    bus.subscribe_at(&listener, one(&[("type", "io.example.paid")]), at(50))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        bus.inbox(&listener)
            .await
            .unwrap_or_else(|error| panic!("{error}"))
            .is_empty()
    );
    let landed = bus
        .publish_at(
            event("paid-3", "io.example.paid", json!({"order": 3})),
            at(60),
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(landed, vec![listener.clone()]);
}

#[tokio::test]
async fn a_waking_publish_flips_only_an_unarmed_events_wait_and_a_second_wake_is_a_no_op() {
    if !database_available() {
        return;
    }
    let database = Database::create().await;
    let bus = database.waking_bus().await;
    let store = database.store().await;
    let waiting = instance_id();
    let armed = instance_id();
    let running = instance_id();
    let joining = instance_id();
    store
        .create(waiting_instance(
            waiting.clone(),
            Status::Waiting(WaitReason::Events),
            None,
        ))
        .await
        .unwrap();
    store
        .create(waiting_instance(
            armed.clone(),
            Status::Waiting(WaitReason::Events),
            Some(at(5)),
        ))
        .await
        .unwrap();
    store
        .create(waiting_instance(running.clone(), Status::Running, None))
        .await
        .unwrap();
    store
        .create(waiting_instance(
            joining.clone(),
            Status::Waiting(WaitReason::Join),
            None,
        ))
        .await
        .unwrap();
    for id in [&waiting, &armed, &running, &joining] {
        bus.subscribe_at(id, one(&[("type", "io.example.paid")]), at(10))
            .await
            .unwrap_or_else(|error| panic!("{error}"));
    }

    let landed = bus
        .publish_at(
            event("paid-1", "io.example.paid", json!({"order": 7})),
            at(40),
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        landed.len(),
        4,
        "every live subscription receives the event"
    );
    let flipped = store.get(&waiting).await.unwrap();
    assert_eq!(flipped.status, Status::Runnable);
    assert_eq!(flipped.wake_at, Some(at(40)));
    assert_eq!(flipped.version, 1);
    assert_eq!(flipped.updated_at, at(40));
    let still_armed = store.get(&armed).await.unwrap();
    assert_eq!(still_armed.status, Status::Waiting(WaitReason::Events));
    assert_eq!(still_armed.wake_at, Some(at(5)));
    assert_eq!(still_armed.version, 0);
    assert_eq!(store.get(&running).await.unwrap().status, Status::Running);
    assert_eq!(
        store.get(&joining).await.unwrap().status,
        Status::Waiting(WaitReason::Join)
    );

    let again = store
        .wake(&[waiting.clone(), armed.clone()], at(80))
        .await
        .unwrap();
    assert_eq!(again, 0, "the publish already armed the waiting row");
    assert_eq!(store.get(&waiting).await.unwrap().version, 1);
    assert_eq!(store.get(&armed).await.unwrap().version, 0);
}

#[tokio::test]
async fn an_unsubscribe_between_match_and_insert_does_not_refill_the_inbox() {
    if !database_available() {
        return;
    }
    let database = Database::create().await;
    let bus = database.bus().await;
    let listener = instance_id();
    bus.subscribe_at(&listener, one(&[("type", "io.example.paid")]), at(10))
        .await
        .unwrap_or_else(|error| panic!("{error}"));

    // Hold the subscription row the way `publish` does, then let unsubscribe
    // run on another connection. It blocks on that lock. Releasing the lock
    // lets unsubscribe delete the inbox; the insert that follows must see
    // `is_active = 0` and write nothing. Without the predicate, this insert
    // would restore the event unsubscribe just deleted.
    let holder = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database.url)
        .await
        .expect("holder");
    let mut tx = holder.begin().await.expect("begin");
    sqlx::query("SELECT id FROM event_subscription WHERE instance_id = $1 FOR UPDATE")
        .bind(sqlx::types::Uuid::parse_str(listener.as_str()).unwrap())
        .execute(&mut *tx)
        .await
        .expect("lock subscription");
    let unsub = database.bus().await;
    let listener_for_unsub = listener.clone();
    let unsubscribe = tokio::spawn(async move {
        unsub
            .unsubscribe_at(&listener_for_unsub, at(30))
            .await
            .unwrap_or_else(|error| panic!("{error}"));
    });
    let started = std::time::Instant::now();
    loop {
        if unsubscribe.is_finished() {
            break;
        }
        if started.elapsed() >= std::time::Duration::from_millis(200) {
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        !unsubscribe.is_finished(),
        "unsubscribe must wait on the subscription row lock"
    );
    // A publish that started while the row was still active, and that inserts
    // only after unsubscribe has the lock, must not restore the inbox. This
    // is that insert: it locks the row, re-reads `is_active`, and writes only
    // when the subscription is still live.
    let publisher = database.bus().await;
    let listener_for_publish = listener.clone();
    let publish = tokio::spawn(async move {
        publisher
            .publish_at(
                event("late-paid", "io.example.paid", json!({"late": true})),
                at(40),
            )
            .await
            .unwrap_or_else(|error| panic!("{error}"))
    });
    let publish_started = std::time::Instant::now();
    loop {
        if publish.is_finished() {
            break;
        }
        if publish_started.elapsed() >= std::time::Duration::from_millis(200) {
            break;
        }
        std::thread::yield_now();
    }
    assert!(
        !publish.is_finished(),
        "publish must wait on the subscription row lock held by unsubscribe"
    );
    tx.commit().await.expect("release lock");
    unsubscribe.await.expect("unsubscribe task");
    let landed = publish.await.expect("publish task");
    assert!(
        landed.is_empty(),
        "publish after unsubscribe does not report a landing"
    );
    assert!(
        bus.inbox(&listener_for_publish)
            .await
            .unwrap_or_else(|error| panic!("{error}"))
            .is_empty(),
        "an insert after unsubscribe does not refill the inbox"
    );
    holder.close().await;
}

#[tokio::test]
async fn a_non_uuid_instance_id_is_a_backend_error() {
    if !database_available() {
        return;
    }
    let database = Database::create().await;
    let bus = database.bus().await;
    let empty = instance_id();
    bus.subscribe_at(&empty, one(&[]), at(10))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let landed = bus
        .publish_at(event("bare", "io.example.paid", json!({})), at(20))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        landed.is_empty(),
        "a filter with no attributes matches nothing"
    );

    let error = bus
        .subscribe(
            &InstanceId::from_string("not-a-uuid"),
            one(&[("type", "io.example.paid")]),
        )
        .await
        .expect_err("instance ids are UUIDs at the SQL boundary");
    let message = error.to_string();
    assert!(
        message.contains("not-a-uuid"),
        "the bad id should be in the error, got {message}"
    );
}

#[tokio::test]
async fn trait_writes_use_the_injected_clock_and_ack_refreshes_updated_at() {
    if !database_available() {
        return;
    }
    let database = Database::create().await;
    let clock = Arc::new(ManualClock::at(at(1_000)));
    let bus = database.bus().await.with_clock(clock.clone());
    let listener = instance_id();
    bus.subscribe(&listener, one(&[("type", "io.example.paid")]))
        .await
        .expect("subscribe");
    clock.set(at(2_000));
    let landed = bus
        .publish(event("clocked", "io.example.paid", json!({})))
        .await
        .expect("publish");
    assert_eq!(landed, vec![listener.clone()]);
    clock.set(at(3_000));
    bus.ack(&listener, &["clocked".to_string()])
        .await
        .expect("ack");

    let (created, updated): (OffsetDateTime, OffsetDateTime) =
        sqlx::query_as("SELECT created_at, updated_at FROM event_inbox WHERE event_id = $1")
            .bind("clocked")
            .fetch_one(bus.pool())
            .await
            .expect("timestamped inbox row");
    assert_eq!(created.unix_timestamp_nanos(), 2_000_000_000);
    assert_eq!(updated.unix_timestamp_nanos(), 3_000_000_000);
}
