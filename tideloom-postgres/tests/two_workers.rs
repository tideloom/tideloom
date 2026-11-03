#![cfg(unix)]
// Copyright 2024 Tideloom Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! M3 exit check: two worker processes share one PostgreSQL database.
//!
//! Worker A runs the root through `hear` and exits gracefully. Worker B then
//! releases the listener, starts `tell`, and blocks in its emit while the test
//! holds an insert-only inbox barrier. The committed `Pending` row and
//! `worker-b` lease are observable before the barrier is released and the root
//! completes.

use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::TcpListener;
use std::net::TcpStream;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use serde_json::json;
use sqlx::types::Uuid;
use tideloom_core::Definition;
use tideloom_core::Timestamp;
use tideloom_postgres::PgDefinitions;
use tideloom_postgres::PgEventBus;
use tideloom_postgres::PgStore;
use tideloom_postgres::PgWakeup;
use tideloom_runtime::Clock;
use tideloom_runtime::CloudEvent;
use tideloom_runtime::Hint;
use tideloom_runtime::Instance;
use tideloom_runtime::InstanceId;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::Status;
use tideloom_runtime::SystemClock;
use tideloom_runtime::WaitReason;
use tideloom_runtime::Wakeup;

const SPEC: &str = include_str!("../../tideloom-runtime/examples/single_process.yaml");
const WORKER_A: &str = "worker-a";
const WORKER_B: &str = "worker-b";
const TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);

fn now() -> Timestamp {
    SystemClock.now()
}

#[tokio::test]
async fn two_workers_hand_off_the_acceptance_workflow_through_postgres() {
    if std::env::var_os("TIDELOOM_TEST_DATABASE_URL").is_none() {
        eprintln!("skipping two-worker test: TIDELOOM_TEST_DATABASE_URL is unset");
        return;
    }

    let server = PayServer::start();
    let database = Database::create().await;
    let store = PgStore::connect(&database.url)
        .await
        .expect("connect store");
    let definitions = PgDefinitions::connect(&database.url)
        .await
        .expect("connect definitions");
    let events = PgEventBus::connect(&database.url)
        .await
        .expect("connect non-waking event bus");
    let wakeup = PgWakeup::connect(&database.url, "s6-two-workers")
        .await
        .expect("connect wakeup");

    let documents = publishable(&server.base);
    let mut root_key = None;
    for document in documents {
        let is_root = document.identity().name() == "single-process";
        let key = definitions.publish(document, now()).await.expect("publish");
        if is_root {
            root_key = Some(key);
        }
    }
    let root = Instance::new(
        root_key.expect("single-process document"),
        None,
        json!({"order": "A-1"}),
        now(),
    );
    let root_id = root.id.clone();
    store.create(root).await.expect("create root");

    let worker_a = Worker::start(&database.url, WORKER_A);
    wait_for_status(
        &store,
        &root_id,
        Status::Waiting(WaitReason::Events),
        "worker A should park the root at hear",
    )
    .await;
    let status = worker_a.stop_gracefully().expect("stop worker A");
    assert!(status.success(), "worker A did not exit cleanly: {status}");

    let worker_b = Worker::start(&database.url, WORKER_B);
    let event_id = "external-paid-a-1";
    let landed = events
        .publish_at(
            CloudEvent {
                id: event_id.to_string(),
                r#type: "io.example.paid".to_string(),
                source: "https://example.test/orders".to_string(),
                subject: None,
                time: None,
                extensions: Default::default(),
                data: json!({"order": "A-1"}),
            },
            now(),
        )
        .await
        .expect("publish external event");
    assert_eq!(landed, vec![root_id.clone()]);

    sqlx::query(
        "CREATE FUNCTION block_s6_inbox_insert() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN PERFORM pg_advisory_xact_lock(728450); RETURN NEW; END $$",
    )
    .execute(store.pool())
    .await
    .expect("create inbox barrier function");
    sqlx::query(
        "CREATE TRIGGER s6_block_inbox_insert BEFORE INSERT ON event_inbox \
         FOR EACH ROW EXECUTE FUNCTION block_s6_inbox_insert()",
    )
    .execute(store.pool())
    .await
    .expect("create inbox insert barrier");
    let mut inbox_lock = store.pool().acquire().await.expect("inbox lock connection");
    sqlx::query("SELECT pg_advisory_lock(728450)")
        .execute(&mut *inbox_lock)
        .await
        .expect("lock inbox inserts");
    let woken = store.wake(&landed, now()).await.expect("wake listener");
    assert_eq!(woken, 1);
    wakeup
        .notify(root_id.clone(), Hint::Event(event_id.to_string()))
        .await
        .expect("notify worker B");

    wait_for_pending_tell(&store, &root_id).await;
    let unlocked = sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock(728450)")
        .fetch_one(&mut *inbox_lock)
        .await
        .expect("release inbox insert barrier");
    assert!(unlocked, "the test held the inbox insert barrier");

    let completed = wait_for_status(
        &store,
        &root_id,
        Status::Completed,
        "worker B should complete the root after the inbox lock is released",
    )
    .await;
    assert_eq!(
        completed.output,
        Some(json!({"order": "A-1", "sealed": true}))
    );
    assert_eq!(server.hits(), 2, "one failed attempt, then one success");

    let status = worker_b.stop_gracefully().expect("stop worker B");
    assert!(status.success(), "worker B did not exit cleanly: {status}");
}

fn publishable(base: &str) -> Vec<Definition> {
    let documents = SPEC
        .replace("{base}", base)
        .split("\n---\n")
        .map(|document| Definition::from_yaml(document).unwrap_or_else(|error| panic!("{error}")))
        .collect::<Vec<_>>();
    assert_eq!(documents.len(), 3, "root, receipt, and tell documents");
    documents
}

async fn wait_for_status(
    store: &PgStore,
    id: &InstanceId,
    expected: Status,
    description: &str,
) -> Instance {
    let started = Instant::now();
    loop {
        let instance = store.get(id).await.expect("read instance");
        if instance.status == expected {
            return instance;
        }
        assert!(
            !matches!(instance.status, Status::Faulted | Status::Cancelled),
            "{description}: {instance:?}"
        );
        assert!(started.elapsed() < TIMEOUT, "{description}: {instance:?}");
        tokio::time::sleep(POLL).await;
    }
}

async fn wait_for_pending_tell(store: &PgStore, root: &InstanceId) {
    let root = Uuid::parse_str(root.as_str()).expect("root UUID");
    let started = Instant::now();
    loop {
        let pending = sqlx::query_scalar::<_, Uuid>(
            "SELECT i.id \
             FROM workflow_instance AS i \
             JOIN task_execution AS t ON t.instance_id = i.id \
             WHERE i.parent_id = $1 \
               AND i.definition_name = 'tell' \
               AND i.lease_worker = $2 \
               AND i.in_flight_key IS NOT NULL \
               AND t.kind = 6 \
               AND t.outcome = 0",
        )
        .bind(root)
        .bind(WORKER_B)
        .fetch_optional(store.pool())
        .await
        .expect("read tell lease and pending emit");
        if pending.is_some() {
            return;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "worker B should retain tell's lease with its emit Pending"
        );
        tokio::time::sleep(POLL).await;
    }
}

struct Worker {
    child: Option<Child>,
}

impl Worker {
    fn start(database_url: &str, worker_id: &str) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_worker"))
            .env("DATABASE_URL", database_url)
            .env("WORKER_ID", worker_id)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|error| panic!("start {worker_id}: {error}"));
        Self { child: Some(child) }
    }

    fn stop_gracefully(mut self) -> std::io::Result<ExitStatus> {
        let child = self.child.as_mut().expect("worker is running");
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "kill -TERM {} failed: {status}",
                child.id()
            )));
        }
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                self.child.take();
                return Ok(status);
            }
            if started.elapsed() >= TIMEOUT {
                return Err(std::io::Error::other(
                    "worker did not exit within 30 seconds of SIGTERM",
                ));
            }
            thread::sleep(POLL);
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
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
}

impl Drop for Database {
    fn drop(&mut self) {
        let url = std::env::var("TIDELOOM_TEST_DATABASE_URL").expect("url");
        let name = self.name.clone();
        thread::spawn(move || {
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

struct PayServer {
    base: String,
    hits: Arc<AtomicUsize>,
}

impl PayServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_thread = Arc::clone(&hits);
        thread::Builder::new()
            .name("two-workers-pay-fixture".to_string())
            .spawn(move || {
                for incoming in listener.incoming() {
                    let Ok(stream) = incoming else {
                        break;
                    };
                    let seen = hits_thread.fetch_add(1, Ordering::SeqCst);
                    let _ = respond(stream, seen);
                }
            })
            .expect("fixture thread");
        Self {
            base: format!("http://127.0.0.1:{port}"),
            hits,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn respond(mut stream: TcpStream, seen: usize) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let _ = read_request(&mut stream)?;
    let (status, reason, body) = if seen == 0 {
        (
            503,
            "Service Unavailable",
            br#"{"error":"down"}"#.as_slice(),
        )
    } else {
        (200, "OK", br#"{"paid":true}"#.as_slice())
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    Ok(buffer)
}
