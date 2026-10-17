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

//! M2 exit check: one workflow, one process, one `Engine`.
//!
//! The document in `single_process.yaml` contains `set`, `call: http`, a `try`
//! that retries, `wait`, a cooperative `fork`, a compete `fork`,
//! `run: workflow`, and an `emit` that satisfies a later `listen`. This example
//! publishes that document, creates one instance, and drives it with
//! [`Engine::drive_until_idle`]. The clock is a [`ManualClock`]. The process
//! does not sleep. HTTP goes to a TCP listener in this process through a
//! registered `HttpExecutor`; nothing here replaces that client or
//! reimplements a step.

use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::TcpListener;
use std::net::TcpStream;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use serde_json::json;
use tideloom_core::Definition;
use tideloom_runtime::Clock;
use tideloom_runtime::EmitExecutor;
use tideloom_runtime::Engine;
use tideloom_runtime::EngineParts;
use tideloom_runtime::EventSink;
use tideloom_runtime::EventSource;
use tideloom_runtime::HttpExecutor;
use tideloom_runtime::Instance;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::LocalEventBus;
use tideloom_runtime::LocalWakeup;
use tideloom_runtime::ManualClock;
use tideloom_runtime::MemoryDefinitions;
use tideloom_runtime::MemoryStore;
use tideloom_runtime::Status;
use tideloom_runtime::Wakeup;
use tideloom_runtime::WorkerId;

const SPEC: &str = include_str!("single_process.yaml");

fn main() {
    let report = run();
    println!("{report}");
}

/// Run the acceptance workflow and return the parent output as JSON text.
fn run() -> String {
    let server = PayServer::start();
    let documents = publishable(&server.base);
    let definitions = Arc::new(MemoryDefinitions::new());
    let mut parent_key = None;
    for document in documents {
        let key = definitions.publish(document);
        if key.name == "single-process" {
            parent_key = Some(key);
        }
    }
    let parent_key = parent_key.expect("single-process document");

    let store = Arc::new(MemoryStore::new());
    let wakeup = Arc::new(LocalWakeup::new(NonZeroUsize::new(64).expect("64")));
    let bus = Arc::new(LocalEventBus::connected(
        Arc::clone(&wakeup) as Arc<dyn Wakeup>
    ));
    let clock = ManualClock::at(tideloom_core::Timestamp::from_millis(1_000));
    let engine = Engine::new(EngineParts {
        worker: WorkerId("single-process".to_string()),
        store: Arc::clone(&store) as Arc<dyn InstanceStore>,
        wakeup: Arc::clone(&wakeup) as Arc<dyn Wakeup>,
        definitions: Arc::clone(&definitions) as Arc<dyn tideloom_runtime::DefinitionSource>,
        executors: vec![
            Arc::new(HttpExecutor::new(
                Arc::clone(&definitions) as Arc<dyn tideloom_runtime::DefinitionSource>
            )),
            Arc::new(EmitExecutor::new(
                Arc::clone(&bus) as Arc<dyn EventSink>,
                Arc::clone(&definitions) as Arc<dyn tideloom_runtime::DefinitionSource>,
            )),
        ],
        events_out: Arc::clone(&bus) as Arc<dyn EventSink>,
        events_in: bus as Arc<dyn EventSource>,
        clock: Arc::new(ManualClock::at(clock.now())),
    });

    let instance = Instance::new(parent_key, None, json!({"order": "A-1"}), clock.now());
    let id = instance.id.clone();
    block_on(store.create(instance)).expect("create");
    let stepped = block_on(engine.drive_until_idle(&clock)).expect("drive");
    assert!(stepped > 0, "the engine claimed nothing");

    let parent = block_on(store.get(&id)).expect("parent");
    assert_eq!(parent.status, Status::Completed, "{parent:?}");
    assert_eq!(
        parent
            .output
            .as_ref()
            .and_then(|output| output.get("sealed")),
        Some(&json!(true)),
        "{parent:?}"
    );
    // The first POST is the fault that `try` retries. The second is the
    // success. A third would mean the retry ran the call twice more.
    assert_eq!(server.hits(), 2, "one failed attempt, then one success");
    assert!(
        clock.now().millis() > 1_000,
        "drive_until_idle advances the injected clock instead of sleeping"
    );

    let children = block_on(store.children(&id)).expect("children");
    let workflows: Vec<_> = children
        .iter()
        .filter(|child| {
            child
                .parent
                .as_ref()
                .is_some_and(|parent| parent.branch.is_none())
        })
        .collect();
    assert_eq!(workflows.len(), 2, "receipt and tell, {children:?}");
    let receipt = workflows
        .iter()
        .find(|child| child.definition.name == "receipt")
        .expect("run: workflow receipt");
    assert_eq!(receipt.status, Status::Completed, "{receipt:?}");
    assert_eq!(
        receipt.output,
        Some(json!({"order": "A-1", "receipt": "A-1"}))
    );
    let tell = workflows
        .iter()
        .find(|child| child.definition.name == "tell")
        .expect("run: workflow tell");
    assert_eq!(tell.status, Status::Completed, "{tell:?}");

    let cooperative: Vec<_> = children
        .iter()
        .filter(|child| {
            child
                .parent
                .as_ref()
                .is_some_and(|parent| parent.branch.is_some())
                && child
                    .output
                    .as_ref()
                    .is_some_and(|output| output.get("lane").is_some())
        })
        .collect();
    assert_eq!(cooperative.len(), 2, "{children:?}");
    assert!(
        cooperative
            .iter()
            .all(|child| child.status == Status::Completed)
    );

    let slow = children
        .iter()
        .find(|child| child.status == Status::Cancelled);
    assert!(
        slow.is_some(),
        "compete cancels the slow branch, {children:?}"
    );

    parent
        .output
        .as_ref()
        .map(|output| serde_json::to_string(output).expect("output"))
        .unwrap_or_else(|| "null".to_string())
}

/// Substitute the fixture address, then split the two workflow documents.
fn publishable(base: &str) -> Vec<Definition> {
    let yaml = SPEC.replace("{base}", base);
    yaml.split("\n---\n")
        .map(|document| Definition::from_yaml(document).unwrap_or_else(|error| panic!("{error}")))
        .collect()
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(future)
}

/// Answers `/pay` with 503 once, then 200. The count is the number of requests.
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
            .name("pay-fixture".to_string())
            .spawn(move || {
                for incoming in listener.incoming() {
                    let Ok(stream) = incoming else {
                        break;
                    };
                    let n = hits_thread.fetch_add(1, Ordering::SeqCst);
                    let _ = respond(stream, n);
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

#[test]
fn the_acceptance_workflow_completes_through_the_engine() {
    let output: serde_json::Value = serde_json::from_str(&run()).expect("json");
    assert_eq!(output["sealed"], true);
    assert_eq!(output["order"], "A-1");
}
