//! Execute `call: http` activities and feed the result back into the result log.
//!
//! [`walk`](crate::walk) stops at the call. [`http_call`] performs that one
//! request. [`drive`] records the output or fault and walks again until the
//! workflow finishes, faults, or stops on a block that is not `call: http`.
//!
//! This slice sends JSON. `endpoint` may be a string or `{ uri }`. Strings
//! accept `${ ... }` interpolation, then simple `{name}` templates filled from
//! the top-level task input. `output` is `content` (the default) or `response`.
//! Status codes outside 200–299 are communication faults. Redirects are not
//! followed.
//!
//! Not in this slice: `output: raw`, endpoint authentication, HTTPS, DSL
//! timeouts, URI-template operators other than `{name}`, and `$item` / `$index`
//! inside `with` (those bindings are not on the block).

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use serde_json::Map;
use serde_json::Value;

use crate::Block;
use crate::CallKind;
use crate::Definition;
use crate::Fault;
use crate::Node;
use crate::NodeKind;
use crate::Outcome;
use crate::Pause;
use crate::ResultLog;
use crate::expr::evaluate_data;
use crate::walk::walk;

const MAX_HTTP_CALLS: u32 = 10_000;
const MAX_BODY: usize = 1024 * 1024;
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Re-walk `definition`, performing each blocked `call: http`.
///
/// Each call's raw output or fault is written into `log` before the next walk,
/// so task `output.as`, `export.as`, and `try` see it. A pause that is not an
/// HTTP activity is returned as [`Outcome::Blocked`] with earlier HTTP results
/// already stored. A `try` retry pause is one of those stops: this function
/// does not sleep for the backoff.
///
/// # Examples
///
/// ```
/// use tideloom_core::Definition;
/// use tideloom_core::Outcome;
/// use tideloom_core::ResultLog;
/// use tideloom_core::drive;
///
/// let yaml = r#"
/// document:
///   dsl: '1.0.0'
///   namespace: default
///   name: paint
///   version: '0.1.0'
/// do:
///   - setColor:
///       set:
///         color: red
/// "#;
/// let definition = Definition::from_yaml(yaml).unwrap();
/// let mut log = ResultLog::new();
/// let Outcome::Completed { output, .. } = drive(&definition, &serde_json::json!({}), &mut log)
/// else {
///     panic!("expected completion");
/// };
/// assert_eq!(output["color"], "red");
/// ```
#[must_use]
pub fn drive(definition: &Definition, input: &Value, log: &mut ResultLog) -> Outcome {
    let mut calls = 0u32;
    loop {
        match walk(definition, input, log) {
            Outcome::Blocked { block } if is_http_activity(&block) => {
                calls += 1;
                if calls > MAX_HTTP_CALLS {
                    return Outcome::Faulted {
                        fault: Fault::runtime(
                            block.position(),
                            format!("HTTP drive exceeded {MAX_HTTP_CALLS} calls"),
                        ),
                        context: block.context().clone(),
                    };
                }
                let key = block.key().clone();
                match http_call(definition, input, &block) {
                    Ok(output) => log.record_output(key, output),
                    Err(fault) => log.record_fault(key, fault),
                }
            }
            other => return other,
        }
    }
}

/// Perform one blocked `call: http` activity.
///
/// `workflow_input` is `$workflow.input`. Task input and `$context` come from
/// `block`. The returned value is the raw call output, before task `output.as`.
///
/// # Errors
///
/// An expression or runtime fault means the `with` block is not a request this
/// slice can build. A communication fault means the call failed or the status
/// was outside 200–299.
pub fn http_call(
    definition: &Definition,
    workflow_input: &Value,
    block: &Block,
) -> Result<Value, Fault> {
    if !is_http_activity(block) {
        return Err(Fault::runtime(
            block.position(),
            "block is not an HTTP activity",
        ));
    }
    let position = block.position();
    let Some(node) = definition.get(position) else {
        return Err(Fault::runtime(
            position,
            "HTTP call node is missing from the definition",
        ));
    };
    let prepared = prepare(node, block.input(), workflow_input, block.context())?;
    let agent = http_agent();
    let received = send(&agent, &prepared)?;
    match prepared.output {
        OutputMode::Content => Ok(received.content),
        OutputMode::Response => Ok(response_value(&prepared, &received)),
    }
}

fn is_http_activity(block: &Block) -> bool {
    block.pause() == Pause::Activity
        && block.effectful()
        && matches!(block.kind(), NodeKind::Call(CallKind::Http))
}

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .allow_non_standard_methods(true)
        .timeout_global(Some(CALL_TIMEOUT))
        .build();
    ureq::Agent::new_with_config(config)
}

enum OutputMode {
    Content,
    Response,
}

struct Prepared {
    position: String,
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    output: OutputMode,
}

struct Received {
    status: u16,
    headers: Map<String, Value>,
    content: Value,
}

fn prepare(
    node: &Node,
    task_input: &Value,
    workflow_input: &Value,
    context: &Value,
) -> Result<Prepared, Fault> {
    let position = node.position();
    let Some(with) = node.body().get("with") else {
        return Err(Fault::runtime(position, "HTTP call is missing `with`"));
    };
    if !with.is_object() {
        return Err(Fault::expression(position, "HTTP `with` must be an object"));
    }
    let vars = scope(node, task_input, workflow_input, context);
    let with = evaluate_data(with, task_input, &vars)
        .map_err(|message| Fault::expression(position, message))?;
    reject_redirect(&with, position)?;
    let output = output_mode(with.get("output"), position)?;
    let method = method_of(with.get("method"), position)?;
    let endpoint = endpoint_uri(with.get("endpoint"), position)?;
    let uri = expand_template(&endpoint, task_input, position)?;
    require_http_url(&uri, position)?;
    let mut headers = header_list(with.get("headers"), position)?;
    let body = match with.get("body") {
        None => None,
        Some(body) => Some(serde_json::to_vec(body).map_err(|error| {
            Fault::runtime(position, format!("failed to encode HTTP body: {error}"))
        })?),
    };
    if body.is_some() && !has_header(&headers, "content-type") {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
    }
    let uri = with_query(&uri, &query_pairs(with.get("query"), position)?);
    Ok(Prepared {
        position: position.to_string(),
        method,
        uri,
        headers,
        body,
        output,
    })
}

fn scope(
    node: &Node,
    task_input: &Value,
    workflow_input: &Value,
    context: &Value,
) -> BTreeMap<String, Value> {
    let mut map = BTreeMap::new();
    map.insert("context".to_string(), context.clone());
    map.insert("input".to_string(), task_input.clone());
    map.insert(
        "workflow".to_string(),
        serde_json::json!({ "input": workflow_input }),
    );
    map.insert(
        "task".to_string(),
        serde_json::json!({
            "name": node.name(),
            "reference": node.position(),
            "input": task_input,
        }),
    );
    map
}

fn reject_redirect(with: &Value, position: &str) -> Result<(), Fault> {
    match with.get("redirect") {
        None | Some(Value::Bool(false)) => Ok(()),
        Some(Value::Bool(true)) => Err(Fault::runtime(
            position,
            "HTTP redirect following is not supported",
        )),
        Some(_) => Err(Fault::expression(
            position,
            "HTTP redirect must be a boolean",
        )),
    }
}

fn output_mode(value: Option<&Value>, position: &str) -> Result<OutputMode, Fault> {
    match value {
        None => Ok(OutputMode::Content),
        Some(Value::String(text)) if text == "content" => Ok(OutputMode::Content),
        Some(Value::String(text)) if text == "response" => Ok(OutputMode::Response),
        Some(Value::String(text)) if text == "raw" => Err(Fault::runtime(
            position,
            "HTTP output `raw` is not supported",
        )),
        Some(Value::String(text)) => Err(Fault::runtime(
            position,
            format!("unknown HTTP output `{text}`"),
        )),
        Some(_) => Err(Fault::expression(position, "HTTP output must be a string")),
    }
}

fn method_of(value: Option<&Value>, position: &str) -> Result<String, Fault> {
    let method = match value {
        None => "GET".to_string(),
        Some(Value::String(text)) => text.to_ascii_uppercase(),
        Some(_) => {
            return Err(Fault::expression(position, "HTTP method must be a string"));
        }
    };
    if method.is_empty() || !method.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return Err(Fault::expression(
            position,
            "HTTP method must be an alphabetic token",
        ));
    }
    Ok(method)
}

fn endpoint_uri(endpoint: Option<&Value>, position: &str) -> Result<String, Fault> {
    let Some(endpoint) = endpoint else {
        return Err(Fault::runtime(position, "HTTP call is missing `endpoint`"));
    };
    match endpoint {
        Value::String(uri) => Ok(uri.clone()),
        Value::Object(object) => {
            if object.contains_key("authentication") {
                return Err(Fault::runtime(
                    position,
                    "HTTP endpoint authentication is not supported",
                ));
            }
            match object.get("uri") {
                Some(Value::String(uri)) => Ok(uri.clone()),
                Some(_) => Err(Fault::expression(position, "endpoint.uri must be a string")),
                None => Err(Fault::runtime(
                    position,
                    "HTTP endpoint object is missing `uri`",
                )),
            }
        }
        _ => Err(Fault::expression(
            position,
            "HTTP endpoint must be a string or an object with `uri`",
        )),
    }
}

fn require_http_url(uri: &str, position: &str) -> Result<(), Fault> {
    if uri.chars().any(|ch| ch.is_whitespace()) {
        return Err(Fault::expression(
            position,
            "HTTP endpoint must not contain whitespace",
        ));
    }
    let lower = uri.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Err(Fault::runtime(position, "HTTPS is not supported"));
    }
    if !lower.starts_with("http://") {
        return Err(Fault::expression(
            position,
            "HTTP endpoint must be an absolute http URL",
        ));
    }
    Ok(())
}

fn header_list(value: Option<&Value>, position: &str) -> Result<Vec<(String, String)>, Fault> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Value::Object(object) = value else {
        return Err(Fault::expression(
            position,
            "HTTP headers must be an object",
        ));
    };
    let mut headers = Vec::with_capacity(object.len());
    for (name, value) in object {
        if name.is_empty() || name.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            return Err(Fault::expression(
                position,
                "HTTP header names must be a single token",
            ));
        }
        let text = scalar_string(value).map_err(|message| {
            Fault::expression(position, format!("HTTP header `{name}` {message}"))
        })?;
        if text.chars().any(|ch| ch == '\r' || ch == '\n') {
            return Err(Fault::expression(
                position,
                format!("HTTP header `{name}` must be a single line"),
            ));
        }
        headers.push((name.clone(), text));
    }
    Ok(headers)
}

fn query_pairs(value: Option<&Value>, position: &str) -> Result<Vec<(String, String)>, Fault> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Value::Object(object) = value else {
        return Err(Fault::expression(position, "HTTP query must be an object"));
    };
    let mut pairs = Vec::new();
    for (name, value) in object {
        match value {
            Value::Array(items) => {
                for item in items {
                    let text = scalar_string(item).map_err(|message| {
                        Fault::expression(position, format!("HTTP query `{name}` {message}"))
                    })?;
                    pairs.push((name.clone(), text));
                }
            }
            other => {
                let text = scalar_string(other).map_err(|message| {
                    Fault::expression(position, format!("HTTP query `{name}` {message}"))
                })?;
                pairs.push((name.clone(), text));
            }
        }
    }
    Ok(pairs)
}

fn scalar_string(value: &Value) -> Result<String, String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(flag) => Ok(flag.to_string()),
        Value::Null => Ok(String::new()),
        _ => Err("must be a string, number, boolean, or null".to_string()),
    }
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case(name))
}

fn with_query(uri: &str, query: &[(String, String)]) -> String {
    if query.is_empty() {
        return uri.to_string();
    }
    let encoded = query
        .iter()
        .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
        .collect::<Vec<_>>()
        .join("&");
    let (base, fragment) = split_fragment(uri);
    if base.contains('?') {
        format!("{base}&{encoded}{fragment}")
    } else {
        format!("{base}?{encoded}{fragment}")
    }
}

fn split_fragment(uri: &str) -> (&str, &str) {
    match uri.find('#') {
        Some(index) => uri.split_at(index),
        None => (uri, ""),
    }
}

/// Simple `{name}` expansion. Names are top-level task-input keys, verbatim.
fn expand_template(uri: &str, input: &Value, position: &str) -> Result<String, Fault> {
    let mut out = String::new();
    let mut rest = uri;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            return Err(Fault::expression(position, "unclosed `{` in HTTP endpoint"));
        };
        let name = after[..end].trim();
        if name.is_empty() {
            return Err(Fault::expression(
                position,
                "empty URI template in HTTP endpoint",
            ));
        }
        if name.starts_with(['+', '#', '.', '/', ';', '?', '&', '=', ',', '!', '@', '|']) {
            return Err(Fault::runtime(
                position,
                "only simple `{name}` URI templates are supported",
            ));
        }
        let value = input.as_object().and_then(|object| object.get(name));
        let text = match value {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(Value::Number(number)) => number.to_string(),
            Some(Value::Bool(flag)) => flag.to_string(),
            Some(_) => {
                return Err(Fault::expression(
                    position,
                    format!("URI template `{{{name}}}` must be a string, number, boolean, or null"),
                ));
            }
        };
        out.push_str(&encode_component(&text));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn send(agent: &ureq::Agent, prepared: &Prepared) -> Result<Received, Fault> {
    let position = prepared.position.as_str();
    let mut builder = ureq::http::Request::builder()
        .method(prepared.method.as_str())
        .uri(prepared.uri.as_str());
    for (name, value) in &prepared.headers {
        builder = builder.header(name, value);
    }
    let body = prepared.body.clone().unwrap_or_default();
    let request = builder
        .body(body)
        .map_err(|error| Fault::expression(position, format!("invalid HTTP request: {error}")))?;
    let mut response = agent
        .run(request)
        .map_err(|error| Fault::communication(position, 500, error.to_string()))?;
    let status = response.status().as_u16();
    let headers = response_headers(response.headers());
    let bytes = read_body(response.body_mut(), position)?;
    if !(200..300).contains(&status) {
        return Err(Fault::communication(
            position,
            status,
            failure_detail(&prepared.method, &prepared.uri, status, &bytes),
        ));
    }
    let content = decode_content(&headers, &bytes, position)?;
    Ok(Received {
        status,
        headers,
        content,
    })
}

fn read_body(body: &mut ureq::Body, position: &str) -> Result<Vec<u8>, Fault> {
    let mut reader = body.as_reader();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader.read(&mut chunk).map_err(|error| {
            Fault::communication(
                position,
                500,
                format!("failed to read HTTP response: {error}"),
            )
        })?;
        if read == 0 {
            return Ok(bytes);
        }
        if bytes.len() + read > MAX_BODY {
            return Err(Fault::communication(
                position,
                500,
                format!("HTTP response body exceeds {MAX_BODY} bytes"),
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

fn response_headers(headers: &ureq::http::HeaderMap) -> Map<String, Value> {
    let mut out = Map::new();
    for (name, value) in headers {
        let Ok(text) = value.to_str() else {
            continue;
        };
        let key = name.as_str().to_string();
        match out.get_mut(&key) {
            Some(Value::String(existing)) => {
                existing.push_str(", ");
                existing.push_str(text);
            }
            _ => {
                out.insert(key, Value::String(text.to_string()));
            }
        }
    }
    out
}

fn decode_content(
    headers: &Map<String, Value>,
    bytes: &[u8],
    position: &str,
) -> Result<Value, Fault> {
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    if json_content(headers) {
        return serde_json::from_slice(bytes).map_err(|error| {
            Fault::communication(position, 500, format!("HTTP response is not JSON: {error}"))
        });
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(Value::String(text.to_string())),
        Err(_) => Err(Fault::runtime(
            position,
            "non-text HTTP response bodies are not supported",
        )),
    }
}

fn json_content(headers: &Map<String, Value>) -> bool {
    let Some(Value::String(value)) = headers.get("content-type") else {
        return false;
    };
    let mime = value.split(';').next().unwrap_or(value).trim();
    mime.eq_ignore_ascii_case("application/json")
        || mime.eq_ignore_ascii_case("text/json")
        || mime.to_ascii_lowercase().ends_with("+json")
}

fn failure_detail(method: &str, uri: &str, status: u16, body: &[u8]) -> String {
    let mut detail = format!("{method} {uri} returned {status}");
    if let Ok(text) = std::str::from_utf8(body) {
        let text = text.trim();
        if !text.is_empty() {
            let snippet: String = text.chars().take(200).collect();
            detail.push_str(": ");
            detail.push_str(&snippet);
        }
    }
    detail
}

fn response_value(prepared: &Prepared, received: &Received) -> Value {
    let mut request_headers = Map::new();
    for (name, value) in &prepared.headers {
        request_headers.insert(name.clone(), Value::String(value.clone()));
    }
    serde_json::json!({
        "request": {
            "method": prepared.method.to_ascii_lowercase(),
            "uri": prepared.uri,
            "headers": request_headers,
        },
        "statusCode": received.status,
        "headers": received.headers,
        "content": received.content,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::expand_template;

    #[test]
    fn simple_template_uses_top_level_task_input() {
        let input = json!({"petId": "1", "pet.id": "literal", "pet": {"id": "nested"}});
        let uri = expand_template(
            "http://example.test/pets/{pet.id}?missing={absent}",
            &input,
            "/do/0/call",
        )
        .unwrap();
        assert_eq!(uri, "http://example.test/pets/literal?missing=");
    }

    #[test]
    fn template_encodes_scalars_and_rejects_objects() {
        let uri = expand_template(
            "http://example.test/{n}/{ok}/{q}",
            &json!({"n": 3, "ok": true, "q": "a b&c"}),
            "/p",
        )
        .unwrap();
        assert_eq!(uri, "http://example.test/3/true/a%20b%26c");
        let error = expand_template(
            "http://example.test/{pet}",
            &json!({"pet": {"id": 1}}),
            "/p",
        )
        .unwrap_err();
        assert_eq!(error.status(), 400);
        let error =
            expand_template("http://example.test/{+q}", &json!({"q": "a"}), "/p").unwrap_err();
        assert!(error.detail().unwrap().contains("simple"));
    }
}
