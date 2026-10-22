//! PgWakeup against a real PostgreSQL.
//!
//! Each test gets its own database. The URL comes from
//! `TIDELOOM_TEST_DATABASE_URL`; a missing URL skips. A notification sent
//! before any session is listening is lost; the row is not, and `watch`
//! returns it from the table.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::StreamExt;
use tideloom_core::Timestamp;
use tideloom_postgres::PgWakeup;
use tideloom_runtime::BusError;
use tideloom_runtime::Hint;
use tideloom_runtime::InstanceId;
use tideloom_runtime::ManualClock;
use tideloom_runtime::Offset;
use tideloom_runtime::Signal;
use tideloom_runtime::SignalStream;
use tideloom_runtime::Wakeup;
use uuid::Uuid;

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

struct Schema {
    url: String,
    name: String,
}

impl Schema {
    async fn create() -> Self {
        let url = std::env::var("TIDELOOM_TEST_DATABASE_URL").unwrap_or_else(|_| {
            panic!("TIDELOOM_TEST_DATABASE_URL is required once a test has started")
        });
        let name = format!("s2_{}", Uuid::now_v7().simple());
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
        let bus = PgWakeup::connect(&test_url, "setup")
            .await
            .expect("connect");
        bus.migrate().await.expect("migrate");
        Self {
            url: test_url,
            name,
        }
    }

    async fn open(&self, group: &str) -> PgWakeup {
        PgWakeup::connect(&self.url, group)
            .await
            .expect("connect")
            .with_clock(Arc::new(ManualClock::at(at(0))))
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
        eprintln!("skipping PgWakeup tests: TIDELOOM_TEST_DATABASE_URL is unset");
        return true;
    }
    false
}

async fn next(stream: &mut SignalStream) -> Signal {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the watcher should not miss a row")
        .expect("the stream should remain open")
        .expect("the offset should still be retained")
}

#[tokio::test]
async fn a_watch_replays_only_later_signals_then_continues_live() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let bus = schema.open("workers").await;
    let instance = InstanceId::new();
    let first = bus.notify(instance.clone(), Hint::Timer).await.unwrap();
    let second = bus.notify(instance.clone(), Hint::Other).await.unwrap();
    assert_eq!(first, Offset(1));
    assert_eq!(second, Offset(2));

    let mut stream = bus.watch(first).await.unwrap();
    assert_eq!(
        next(&mut stream).await,
        Signal {
            instance: instance.clone(),
            hint: Hint::Other,
            offset: second
        }
    );
    assert!(stream.next().now_or_never().is_none());
    let third = bus
        .notify(instance.clone(), Hint::Event("landed".into()))
        .await
        .unwrap();
    assert_eq!(
        next(&mut stream).await,
        Signal {
            instance: instance.clone(),
            hint: Hint::Event("landed".into()),
            offset: third
        }
    );
    let child = InstanceId::new();
    let fourth = bus
        .notify(instance.clone(), Hint::ChildDone(child.clone()))
        .await
        .unwrap();
    assert_eq!(
        next(&mut stream).await,
        Signal {
            instance,
            hint: Hint::ChildDone(child),
            offset: fourth
        }
    );
    assert!(stream.next().now_or_never().is_none());
}

#[tokio::test]
async fn every_waiting_worker_receives_the_same_live_signals() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let bus = schema.open("workers").await;
    let mut first = bus.watch(Offset(0)).await.unwrap();
    let mut second = bus.watch(Offset(0)).await.unwrap();
    for expected in 1..=4 {
        let (ready, mut waiting) = tokio::sync::mpsc::channel(2);
        let [first_worker, second_worker] = [first, second].map(|mut stream| {
            let ready = ready.clone();
            tokio::spawn(async move {
                assert!(stream.next().now_or_never().is_none());
                ready.try_send(()).unwrap();
                let signal = next(&mut stream).await;
                (stream, signal)
            })
        });
        waiting.recv().await.unwrap();
        waiting.recv().await.unwrap();
        let instance = InstanceId::new();
        let offset = bus.notify(instance.clone(), Hint::Other).await.unwrap();
        let signal = Signal {
            instance,
            hint: Hint::Other,
            offset: Offset(expected),
        };
        assert_eq!(offset, signal.offset);
        let (first_stream, first_signal) = first_worker.await.unwrap();
        let (second_stream, second_signal) = second_worker.await.unwrap();
        assert_eq!(first_signal, signal);
        assert_eq!(second_signal, signal);
        first = first_stream;
        second = second_stream;
    }
}

#[tokio::test]
async fn a_cursor_older_than_the_oldest_retained_id_is_expired() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let bus = schema.open("workers").await;
    let instance = InstanceId::new();
    let first = bus.notify(instance.clone(), Hint::Timer).await.unwrap();
    let second = bus.notify(instance.clone(), Hint::Other).await.unwrap();
    assert_eq!(first, Offset(1));
    sqlx::query("DELETE FROM signal_log WHERE id = $1")
        .bind(i64::try_from(first.0).unwrap())
        .execute(bus.pool())
        .await
        .unwrap();
    assert!(matches!(
        bus.watch(Offset(0)).await,
        Err(BusError::OffsetExpired(Offset(0)))
    ));
    // A consumer that already passed the deleted id can still replay the rest.
    let mut stream = bus.watch(first).await.unwrap();
    assert_eq!(next(&mut stream).await.offset, second);
    let mut tip = bus.watch(second).await.unwrap();
    assert!(tip.next().now_or_never().is_none());
}

#[tokio::test]
async fn a_lagging_live_watcher_reports_expiration_then_ends() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let bus = schema.open("workers").await;
    let mut stream = bus.watch(Offset(0)).await.unwrap();
    assert!(stream.next().now_or_never().is_none());
    let first = bus.notify(InstanceId::new(), Hint::Other).await.unwrap();
    let second = bus.notify(InstanceId::new(), Hint::Timer).await.unwrap();
    // Hold the next read until the gap exists. A notification can otherwise
    // deliver `first` before the delete, which is a race, not retention.
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let held = {
        let gate = Arc::clone(&gate);
        tokio::spawn(async move {
            gate.wait().await;
            stream.next().await
        })
    };
    sqlx::query("DELETE FROM signal_log WHERE id = $1")
        .bind(i64::try_from(first.0).unwrap())
        .execute(bus.pool())
        .await
        .unwrap();
    gate.wait().await;
    // The deleted row is the one this cursor has not consumed. The later row
    // must not be delivered as if nothing were missed.
    let _ = second;
    assert!(matches!(
        held.await.unwrap(),
        Some(Err(BusError::OffsetExpired(Offset(0))))
    ));
}

#[tokio::test]
async fn committed_offsets_advance_monotonically_per_consumer_group() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let bus = schema.open("workers").await;
    let other = schema.open("sweeper").await;
    assert_eq!(bus.committed().await.unwrap(), Offset(0));
    for _ in 0..4 {
        bus.notify(InstanceId::new(), Hint::Other).await.unwrap();
    }
    // Exercise the backend contract directly; production acknowledgements
    // go through Engine::commit after the store apply succeeds.
    for offset in [3, 3, 1, 0] {
        bus.commit_offset(Offset(offset)).await.unwrap();
        assert_eq!(bus.committed().await.unwrap(), Offset(3));
    }
    other.commit_offset(Offset(1)).await.unwrap();
    assert_eq!(other.committed().await.unwrap(), Offset(1));
    assert_eq!(bus.committed().await.unwrap(), Offset(3));
    let mut resumed = bus.watch(bus.committed().await.unwrap()).await.unwrap();
    assert_eq!(next(&mut resumed).await.offset, Offset(4));
    let mut independent = bus.watch(Offset(0)).await.unwrap();
    assert_eq!(next(&mut independent).await.offset, Offset(1));
    bus.commit_offset(Offset(4)).await.unwrap();
    assert_eq!(bus.committed().await.unwrap(), Offset(4));
    assert_eq!(other.committed().await.unwrap(), Offset(1));
}

#[tokio::test]
async fn a_notification_sent_before_any_listener_is_still_in_the_log() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let bus = schema.open("workers").await;
    let instance = InstanceId::new();
    // No session is listening. Postgres delivers NOTIFY only to sessions that
    // already LISTEN, so this hint is lost. The row is not.
    let offset = bus
        .notify(instance.clone(), Hint::Event("quiet".into()))
        .await
        .unwrap();
    let mut stream = bus.watch(Offset(0)).await.unwrap();
    assert_eq!(
        next(&mut stream).await,
        Signal {
            instance,
            hint: Hint::Event("quiet".into()),
            offset
        }
    );
}

#[tokio::test]
async fn concurrent_publishers_get_unique_offsets_in_log_order() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let bus = Arc::new(schema.open("workers").await);
    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut published = Vec::new();
    for publisher in 0..8 {
        let bus = Arc::clone(&bus);
        let barrier = Arc::clone(&barrier);
        published.push(tokio::spawn(async move {
            barrier.wait().await;
            let mut signals: Vec<Signal> = Vec::new();
            for sequence in 0..8 {
                let instance = InstanceId::new();
                let hint = Hint::Event(format!("{publisher}:{sequence}"));
                let offset = bus.notify(instance.clone(), hint.clone()).await.unwrap();
                if let Some(previous) = signals.last() {
                    assert!(previous.offset < offset);
                }
                signals.push(Signal {
                    instance,
                    hint,
                    offset,
                });
            }
            signals
        }));
    }
    let mut published = futures::future::try_join_all(published)
        .await
        .unwrap()
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    published.sort_by_key(|signal| signal.offset);
    let mut stream = bus.watch(Offset(0)).await.unwrap();
    for (index, signal) in published.into_iter().enumerate() {
        assert_eq!(signal.offset, Offset(index as u64 + 1));
        assert_eq!(next(&mut stream).await, signal);
    }
    assert!(stream.next().now_or_never().is_none());
}
