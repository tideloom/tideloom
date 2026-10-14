use std::time::Duration;

use serde_json::json;
use tideloom_core::Continuation;
use tideloom_core::Definition;
use tideloom_core::Fault;
use tideloom_core::Outcome;
use tideloom_core::Resume;
use tideloom_core::Timestamp;
use tideloom_core::WalkOptions;
use tideloom_core::resume_with;
use tideloom_core::start;
use tideloom_core::start_with;

const TIMEOUT_TYPE: &str = "https://open-workflow-specification.org/spec/1.0.0/errors/timeout";

fn sample(tasks: &str) -> Definition {
    document("", tasks)
}

fn document(head: &str, tasks: &str) -> Definition {
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: sample\n  version: '0.1.0'\n{head}do:\n{tasks}"
    );
    Definition::from_yaml(&yaml).unwrap_or_else(|error| panic!("{error}"))
}

fn blocked(outcome: Outcome) -> (tideloom_core::Block, Continuation) {
    match outcome {
        Outcome::Blocked {
            block,
            continuation,
        } => (block, continuation),
        other => panic!("expected a block, got {other:?}"),
    }
}

fn faulted(outcome: Outcome) -> Fault {
    match outcome {
        Outcome::Faulted { fault, .. } => fault,
        other => panic!("expected a fault, got {other:?}"),
    }
}

fn at(millis: u64) -> WalkOptions {
    WalkOptions::new().with_now(Timestamp::from_millis(millis))
}

#[test]
fn standard_error_types_use_the_spec_status() {
    let configuration = Fault::configuration("/", "missing");
    assert_eq!(
        configuration.error_type(),
        "https://open-workflow-specification.org/spec/1.0.0/errors/configuration"
    );
    assert_eq!(configuration.status(), 400);

    let validation = Fault::validation("/do/0/check", "schema");
    assert_eq!(
        validation.error_type(),
        "https://open-workflow-specification.org/spec/1.0.0/errors/validation"
    );
    assert_eq!(validation.status(), 400);

    let authentication = Fault::authentication("/do/0/call", "bad token");
    assert_eq!(
        authentication.error_type(),
        "https://open-workflow-specification.org/spec/1.0.0/errors/authentication"
    );
    assert_eq!(authentication.status(), 401);

    let authorization = Fault::authorization("/do/0/call", "forbidden");
    assert_eq!(
        authorization.error_type(),
        "https://open-workflow-specification.org/spec/1.0.0/errors/authorization"
    );
    assert_eq!(authorization.status(), 403);

    let timeout = Fault::timeout("/do/0/charge", "timed out after 5ms");
    assert_eq!(timeout.error_type(), TIMEOUT_TYPE);
    assert_eq!(timeout.status(), 408);
}

#[test]
fn a_workflow_timeout_faults_at_the_deadline_without_sleeping() {
    let definition = document(
        "timeout:\n  after:\n    seconds: 30\n",
        r#"
  - pause:
      wait:
        seconds: 60
"#,
    );
    let started = Timestamp::from_millis(5_000);
    let (waiting, mut continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(waiting.position(), "/do/0/pause");
    assert!(waiting.timeout().is_none());
    assert_eq!(continuation.workflow_started(), None);
    continuation.start_workflow(started);
    continuation.start_workflow(Timestamp::from_millis(9_000));
    assert_eq!(continuation.workflow_started(), Some(started));

    let (waiting, continuation) = blocked(resume_with(
        &definition,
        continuation,
        Resume::Tick,
        at(5_000 + 29_999),
    ));
    assert_eq!(waiting.position(), "/do/0/pause");

    let fault = faulted(resume_with(
        &definition,
        continuation,
        Resume::Tick,
        at(5_000 + 30_000),
    ));
    assert_eq!(fault.error_type(), TIMEOUT_TYPE);
    assert_eq!(fault.status(), 408);
    assert_eq!(fault.instance(), "/");
    assert_eq!(fault.detail(), Some("timed out after 30000ms"));
}

#[test]
fn start_with_a_clock_records_the_workflow_start() {
    let definition = document(
        "timeout:\n  after: PT1S\n",
        r#"
  - pause:
      wait: {}
"#,
    );
    let (_, continuation) = blocked(start_with(&definition, &json!({}), at(10)));
    assert_eq!(
        continuation.workflow_started(),
        Some(Timestamp::from_millis(10))
    );
    let (_, continuation) = blocked(resume_with(
        &definition,
        continuation,
        Resume::Tick,
        at(1_009),
    ));
    let fault = faulted(resume_with(
        &definition,
        continuation,
        Resume::Tick,
        at(1_010),
    ));
    assert_eq!(fault.instance(), "/");
}

#[test]
fn a_named_workflow_timeout_and_an_expression_duration() {
    let named = document(
        r#"
use:
  timeouts:
    quick:
      after: PT1S
timeout: quick
"#,
        r#"
  - pause:
      wait: {}
"#,
    );
    let (_, mut continuation) = blocked(start(&named, &json!({})));
    continuation.start_workflow(Timestamp::from_millis(0));
    let (_, continuation) = blocked(resume_with(&named, continuation, Resume::Tick, at(999)));
    let fault = faulted(resume_with(&named, continuation, Resume::Tick, at(1_000)));
    assert_eq!(fault.instance(), "/");

    let expressed = document(
        "timeout:\n  after: ${ .limit }\n",
        r#"
  - pause:
      wait: {}
"#,
    );
    let (_, mut continuation) = blocked(start(&expressed, &json!({"limit": "PT2S"})));
    continuation.start_workflow(Timestamp::from_millis(10));
    let fault = faulted(resume_with(
        &expressed,
        continuation,
        Resume::Tick,
        at(2_010),
    ));
    assert_eq!(fault.detail(), Some("timed out after 2000ms"));
}

#[test]
fn a_task_timeout_is_caught_and_a_finished_call_is_kept() {
    let definition = sample(
        r#"
  - attempt:
      try:
        - callPay:
            call: http
            with:
              method: get
              endpoint: http://127.0.0.1:1/
            timeout:
              after:
                milliseconds: 5
      catch:
        errors:
          with:
            type: https://open-workflow-specification.org/spec/1.0.0/errors/timeout
            status: 408
        do:
          - recover:
              set:
                ok: true
"#,
    );
    let (call, mut continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(call.position(), "/do/0/attempt/try/0/callPay");
    assert_eq!(call.timeout(), Some(Duration::from_millis(5)));
    let started = Timestamp::from_millis(100);
    assert_eq!(call.timeout_at(started), Some(Timestamp::from_millis(105)));
    continuation.start_task(started);
    continuation.start_task(Timestamp::from_millis(1_000));
    assert_eq!(continuation.task_started(), Some(started));

    let (still, continuation) = blocked(resume_with(
        &definition,
        continuation,
        Resume::Tick,
        at(104),
    ));
    assert_eq!(still.key(), call.key());

    let (output, _) = match resume_with(&definition, continuation.clone(), Resume::Tick, at(105)) {
        Outcome::Completed { output, context } => (output, context),
        other => panic!("expected the catch, got {other:?}"),
    };
    assert_eq!(output, json!({"ok": true}));

    let (output, _) = match resume_with(
        &definition,
        continuation,
        Resume::Output(json!({"paid": true})),
        at(105),
    ) {
        Outcome::Completed { output, context } => (output, context),
        other => panic!("expected the recorded output, got {other:?}"),
    };
    assert_eq!(output, json!({"paid": true}));
}

#[test]
fn a_tick_faults_a_timed_out_call_before_a_runner_connects() {
    let definition = sample(
        r#"
  - callPay:
      call: http
      with:
        method: get
        endpoint: http://127.0.0.1:1/
      timeout:
        after:
          milliseconds: 1
"#,
    );
    let (_, mut continuation) = blocked(start(&definition, &json!({})));
    continuation.start_task(Timestamp::from_millis(0));
    // A tick at the deadline faults before any runner would send the call.
    let fault = faulted(resume_with(&definition, continuation, Resume::Tick, at(1)));
    assert_eq!(fault.error_type(), TIMEOUT_TYPE);
    assert_eq!(fault.instance(), "/do/0/callPay");
}

#[test]
fn a_bad_timeout_faults_instead_of_pausing() {
    let missing = sample(
        r#"
  - pause:
      wait:
        seconds: 1
      timeout: {}
"#,
    );
    let fault = faulted(start(&missing, &json!({})));
    assert_eq!(fault.instance(), "/do/0/pause");
    assert!(
        fault.detail().unwrap_or("").contains("timeout.after"),
        "{fault}"
    );

    let unknown = document(
        "timeout: missing\n",
        r#"
  - paint:
      set:
        color: red
"#,
    );
    let fault = faulted(start(&unknown, &json!({})));
    assert_eq!(fault.instance(), "/");
    assert!(
        fault.detail().unwrap_or("").contains("unknown timeout"),
        "{fault}"
    );
}
