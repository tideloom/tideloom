//! `call: http` against a real listener on 127.0.0.1.
//!
//! The server is a TCP socket in this process. The workflow uses the library
//! HTTP client; nothing here replaces that client with a fake.

use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tideloom_core::Definition;
use tideloom_core::Outcome;
use tideloom_core::ResultLog;
use tideloom_core::drive;

const COMMUNICATION: &str =
    "https://open-workflow-specification.org/spec/1.0.0/errors/communication";

struct TestServer {
    base: String,
    hits: Arc<AtomicUsize>,
}

impl TestServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|error| panic!("{error}"));
        let port = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("{error}"))
            .port();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_thread = Arc::clone(&hits);
        thread::Builder::new()
            .name("http-fixture".to_string())
            .spawn(move || {
                for incoming in listener.incoming() {
                    let Ok(stream) = incoming else {
                        break;
                    };
                    hits_thread.fetch_add(1, Ordering::SeqCst);
                    let _ = respond(stream);
                }
            })
            .unwrap_or_else(|error| panic!("{error}"));
        Self {
            base: format!("http://127.0.0.1:{port}"),
            hits,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn definition(tasks: &str) -> Definition {
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: http-call\n  version: '0.1.0'\ndo:\n{tasks}"
    );
    Definition::from_yaml(&yaml).unwrap_or_else(|error| panic!("{error}"))
}

fn run(tasks: &str, input: Value) -> (Outcome, ResultLog) {
    let definition = definition(tasks);
    let mut log = ResultLog::new();
    let outcome = drive(&definition, &input, &mut log);
    (outcome, log)
}

fn completed(tasks: &str, input: Value) -> Value {
    match run(tasks, input).0 {
        Outcome::Completed { output, .. } => output,
        other => panic!("expected completion, got {other:?}"),
    }
}

fn faulted(tasks: &str, input: Value) -> tideloom_core::Fault {
    match run(tasks, input).0 {
        Outcome::Faulted { fault, .. } => fault,
        other => panic!("expected a fault, got {other:?}"),
    }
}

fn header<'a>(headers: &'a Value, name: &str) -> &'a Value {
    headers
        .as_object()
        .and_then(|object| {
            object
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value)
        })
        .unwrap_or(&Value::Null)
}

#[test]
fn content_output_feeds_the_next_walk() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - fetch:
      call: http
      with:
        method: get
        endpoint: {base}/pets/milou
        output: content
  - stamp:
      set:
        stamped: true
"#,
        base = server.base
    );
    let output = completed(&tasks, json!({}));
    assert_eq!(
        output,
        json!({"id": "milou", "name": "milou", "status": "pending", "stamped": true})
    );
    assert_eq!(server.hits(), 1);
}

#[test]
fn a_finished_call_is_not_repeated_from_the_same_log() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - fetch:
      call: http
      with:
        method: get
        endpoint: {base}/pets/milou
"#,
        base = server.base
    );
    let definition = definition(&tasks);
    let mut log = ResultLog::new();
    let first = drive(&definition, &json!({}), &mut log);
    assert!(matches!(first, Outcome::Completed { .. }));
    assert_eq!(server.hits(), 1);
    let second = drive(&definition, &json!({}), &mut log);
    assert_eq!(first, second);
    assert_eq!(server.hits(), 1);
}

#[test]
fn endpoint_object_template_and_task_output() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - find:
      call: http
      with:
        endpoint:
          uri: {base}/pets/{{petId}}
      output:
        as: .id
"#,
        base = server.base
    );
    let output = completed(&tasks, json!({"petId": "a b"}));
    assert_eq!(output, json!("a b"));
    assert_eq!(server.hits(), 1);
}

#[test]
fn endpoint_string_interpolation_uses_task_input() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - find:
      input:
        from: '{{ petId: .id }}'
      call: http
      with:
        method: get
        endpoint: {base}/pets/${{ .petId }}
"#,
        base = server.base
    );
    let output = completed(&tasks, json!({"id": "4", "extra": true}));
    assert_eq!(
        output,
        json!({"id": "4", "name": "milou", "status": "pending"})
    );
}

#[test]
fn post_sends_json_body_headers_and_query() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - submit:
      call: http
      with:
        method: post
        endpoint: {base}/echo
        headers:
          x-trace: ${{ .trace }}
        query:
          q: ${{ .q }}
        body:
          sku: ${{ .sku }}
          qty: 2
"#,
        base = server.base
    );
    let output = completed(&tasks, json!({"trace": "abc", "q": "a b", "sku": "pen"}));
    assert_eq!(output["method"], "POST");
    assert_eq!(output["path"], "/echo");
    assert_eq!(output["query"], "q=a%20b");
    assert_eq!(header(&output["headers"], "x-trace"), &json!("abc"));
    assert_eq!(
        header(&output["headers"], "content-type"),
        &json!("application/json")
    );
    assert_eq!(output["body"], json!({"qty": 2, "sku": "pen"}));
}

#[test]
fn response_output_includes_request_status_headers_and_content() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - find:
      call: http
      with:
        method: get
        endpoint:
          uri: {base}/pets/{{petId}}
        headers:
          x-trace: pet
        query:
          verbose: "yes"
        output: response
"#,
        base = server.base
    );
    let output = completed(&tasks, json!({"petId": "milou"}));
    assert_eq!(output["statusCode"], 200);
    assert_eq!(output["request"]["method"], "get");
    assert_eq!(output["request"]["headers"]["x-trace"], "pet");
    let uri = output["request"]["uri"].as_str().unwrap_or("");
    assert!(uri.contains("/pets/milou"), "{uri}");
    assert!(uri.contains("verbose=yes"), "{uri}");
    assert_eq!(
        output["content"],
        json!({"id": "milou", "name": "milou", "status": "pending"})
    );
    assert_eq!(
        header(&output["headers"], "content-type"),
        &json!("application/json")
    );
}

#[test]
fn plain_text_content_is_a_string() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - read:
      call: http
      with:
        method: get
        endpoint: {base}/plain
"#,
        base = server.base
    );
    assert_eq!(completed(&tasks, json!({})), json!("hello"));
}

#[test]
fn non_success_status_is_a_communication_fault() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - fetch:
      call: http
      with:
        method: get
        endpoint: {base}/missing
"#,
        base = server.base
    );
    let fault = faulted(&tasks, json!({}));
    assert_eq!(fault.error_type(), COMMUNICATION);
    assert_eq!(fault.status(), 404);
    assert_eq!(fault.instance(), "/do/0/fetch");
    assert!(fault.detail().unwrap_or("").contains("404"));
}

#[test]
fn redirects_are_not_followed() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - fetch:
      call: http
      with:
        method: get
        endpoint: {base}/redirect
"#,
        base = server.base
    );
    let fault = faulted(&tasks, json!({}));
    assert_eq!(fault.status(), 302);
    assert_eq!(server.hits(), 1);
}

#[test]
fn invalid_json_content_faults() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - fetch:
      call: http
      with:
        method: get
        endpoint: {base}/bad-json
"#,
        base = server.base
    );
    let fault = faulted(&tasks, json!({}));
    assert_eq!(fault.error_type(), COMMUNICATION);
    assert!(fault.detail().unwrap_or("").contains("not JSON"));
}

#[test]
fn try_catches_the_recorded_http_fault() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - attempt:
      try:
        - fetch:
            call: http
            with:
              method: get
              endpoint: {base}/missing
      catch:
        do:
          - recover:
              set:
                status: ${{ $error.status }}
                kind: ${{ $error.type }}
"#,
        base = server.base
    );
    let output = completed(&tasks, json!({}));
    assert_eq!(output["status"], 404);
    assert_eq!(output["kind"], COMMUNICATION);
}

#[test]
fn a_false_if_does_not_call() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - maybe:
      if: .go
      call: http
      with:
        method: get
        endpoint: {base}/echo
  - after:
      set:
        ran: true
"#,
        base = server.base
    );
    let output = completed(&tasks, json!({"go": false}));
    assert_eq!(output, json!({"go": false, "ran": true}));
    assert_eq!(server.hits(), 0);
}

#[test]
fn unsupported_shapes_fail_before_connecting() {
    let raw = faulted(
        r#"
  - fetch:
      call: http
      with:
        method: get
        endpoint: http://127.0.0.1:9/nope
        output: raw
"#,
        json!({}),
    );
    assert_eq!(raw.status(), 500);
    assert!(raw.detail().unwrap_or("").contains("raw"));

    let auth = faulted(
        r#"
  - fetch:
      call: http
      with:
        method: get
        endpoint:
          uri: http://127.0.0.1:9/nope
          authentication:
            basic:
              username: a
              password: b
"#,
        json!({}),
    );
    assert!(auth.detail().unwrap_or("").contains("authentication"));

    let https = faulted(
        r#"
  - fetch:
      call: http
      with:
        method: get
        endpoint: https://example.test/pets
"#,
        json!({}),
    );
    assert!(https.detail().unwrap_or("").contains("HTTPS"));

    let redirect = faulted(
        r#"
  - fetch:
      call: http
      with:
        method: get
        endpoint: http://127.0.0.1:9/nope
        redirect: true
"#,
        json!({}),
    );
    assert!(redirect.detail().unwrap_or("").contains("redirect"));
}

#[test]
fn an_object_template_value_does_not_connect() {
    let server = TestServer::start();
    let tasks = format!(
        r#"
  - fetch:
      call: http
      with:
        method: get
        endpoint: {base}/pets/{{pet}}
"#,
        base = server.base
    );
    let fault = faulted(&tasks, json!({"pet": {"id": 1}}));
    assert_eq!(fault.status(), 400);
    assert_eq!(server.hits(), 0);
}

#[test]
fn drive_leaves_a_non_http_activity_blocked() {
    let (outcome, log) = run(
        r#"
  - notify:
      emit:
        event:
          with:
            source: https://example.test/paint
            type: io.example.painted
"#,
        json!({}),
    );
    let Outcome::Blocked { block } = outcome else {
        panic!("expected a block");
    };
    assert_eq!(block.position(), "/do/0/notify");
    assert!(log.effect(block.key()).is_none());
}

fn respond(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let (headers, body) = read_request(&mut stream)?;
    let mut lines = headers.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path == "/redirect" {
        let head = "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/nowhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream.write_all(head.as_bytes())?;
        stream.flush()?;
        let _ = stream.shutdown(Shutdown::Both);
        return Ok(());
    }
    if path == "/plain" {
        return write_response(&mut stream, 200, "OK", "text/plain", b"hello");
    }
    if path == "/bad-json" {
        return write_response(&mut stream, 200, "OK", "application/json", b"not-json");
    }
    if path == "/missing" {
        return write_response(
            &mut stream,
            404,
            "Not Found",
            "application/json",
            br#"{"error":"missing"}"#,
        );
    }
    if let Some(id) = path.strip_prefix("/pets/")
        && !id.is_empty()
    {
        let payload = json!({"id": percent_decode(id), "name": "milou", "status": "pending"});
        return write_response(
            &mut stream,
            200,
            "OK",
            "application/json",
            &serde_json::to_vec(&payload).unwrap_or_default(),
        );
    }
    if path == "/echo" {
        let parsed_body = if body.is_empty() {
            Value::Null
        } else if header_value(&headers, "content-type")
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("json")
        {
            serde_json::from_slice(&body).unwrap_or(Value::Null)
        } else {
            Value::String(String::from_utf8_lossy(&body).into_owned())
        };
        let mut echoed_headers = serde_json::Map::new();
        for line in headers.lines().skip(1) {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            echoed_headers.insert(
                name.trim().to_ascii_lowercase(),
                Value::String(value.trim().to_string()),
            );
        }
        let payload = json!({
            "method": method,
            "path": path,
            "query": query,
            "headers": echoed_headers,
            "body": parsed_body,
        });
        return write_response(
            &mut stream,
            200,
            "OK",
            "application/json",
            &serde_json::to_vec(&payload).unwrap_or_default(),
        );
    }
    write_response(
        &mut stream,
        404,
        "Not Found",
        "application/json",
        br#"{"error":"missing"}"#,
    )
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        if let Some(header_end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
            let length = content_length(&header_text);
            let total = header_end + 4 + length;
            if buf.len() >= total {
                let body = buf[header_end + 4..total].to_vec();
                return Ok((header_text, body));
            }
        }
        if buf.len() > 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
        match stream.read(&mut tmp) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "request ended early",
                ));
            }
            Ok(read) => buf.extend_from_slice(&tmp[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn content_length(headers: &str) -> usize {
    header_value(headers, "content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
