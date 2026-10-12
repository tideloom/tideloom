use serde_json::json;
use tideloom_core::Continuation;
use tideloom_core::Definition;
use tideloom_core::Outcome;
use tideloom_core::Pause;
use tideloom_core::Resume;
use tideloom_core::Timestamp;
use tideloom_core::WalkOptions;
use tideloom_core::resume;
use tideloom_core::resume_with;
use tideloom_core::start;

fn sample(tasks: &str) -> Definition {
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: sample\n  version: '0.1.0'\ndo:\n{tasks}"
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

fn at(millis: u64) -> WalkOptions {
    WalkOptions::new().with_now(Timestamp::from_millis(millis))
}

#[test]
fn attempt_duration_times_out_one_try_then_retries() {
    let definition = sample(
        r#"
  - charge:
      try:
        - callPay:
            call: http
            with:
              method: get
              endpoint: http://127.0.0.1:1/
      catch:
        retry:
          delay: PT30S
          limit:
            attempt:
              count: 1
              duration:
                milliseconds: 5
"#,
    );
    let (call, mut continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(continuation.task_started(), None);
    continuation.start_task(Timestamp::from_millis(100));

    let (still, continuation) = blocked(resume_with(
        &definition,
        continuation,
        Resume::Tick,
        at(104),
    ));
    assert_eq!(still.key(), call.key());
    assert_eq!(
        continuation.task_started(),
        Some(Timestamp::from_millis(100))
    );

    let (retry, continuation) = blocked(resume_with(
        &definition,
        continuation,
        Resume::Tick,
        at(105),
    ));
    assert_eq!(retry.position(), "/do/0/charge");
    assert_eq!(
        retry.pause(),
        Pause::Retry {
            attempt: 0,
            delay: std::time::Duration::from_secs(30),
        }
    );
    // A new block starts its own clock at `now`.
    assert_eq!(
        continuation.task_started(),
        Some(Timestamp::from_millis(105))
    );

    let (again, _) = blocked(resume_with(
        &definition,
        continuation,
        Resume::Release(json!(null)),
        at(105),
    ));
    assert_eq!(again.position(), call.position());
    assert_eq!(again.execution_key().to_string(), "attempt:1");
}

#[test]
fn retry_duration_stops_the_window_without_sleeping() {
    let definition = sample(
        r#"
  - charge:
      try:
        - callPay:
            call: http
            with:
              method: get
              endpoint: http://127.0.0.1:1/
      catch:
        retry:
          delay: PT30S
          limit:
            attempt:
              count: 5
            duration:
              milliseconds: 10
        do:
          - recover:
              set:
                ok: false
"#,
    );
    let (call, continuation) = blocked(start(&definition, &json!({})));
    let (retry, mut continuation) = blocked(resume(
        &definition,
        continuation,
        Resume::Fault(tideloom_core::Fault::runtime(call.position(), "down")),
    ));
    assert!(matches!(retry.pause(), Pause::Retry { attempt: 0, .. }));
    continuation.start_task(Timestamp::from_millis(1_000));

    let (waiting, continuation) = blocked(resume_with(
        &definition,
        continuation,
        Resume::Tick,
        at(1_009),
    ));
    assert_eq!(waiting.key(), retry.key());

    let (output, _) = match resume_with(&definition, continuation, Resume::Tick, at(1_010)) {
        Outcome::Completed { output, context } => (output, context),
        other => panic!("expected the catch, got {other:?}"),
    };
    assert_eq!(output, json!({"ok": false}));
}

#[test]
fn the_retry_window_is_kept_across_attempts() {
    let definition = sample(
        r#"
  - charge:
      try:
        - callPay:
            call: http
      catch:
        retry:
          limit:
            attempt:
              count: 5
            duration:
              milliseconds: 10
        do:
          - recover:
              set:
                ok: false
"#,
    );
    let (call, continuation) = blocked(start(&definition, &json!({})));
    let down = Resume::Fault(tideloom_core::Fault::runtime(call.position(), "down"));
    let (_, mut continuation) = blocked(resume(&definition, continuation, down.clone()));
    continuation.start_task(Timestamp::from_millis(1_000));

    let (second, _continuation) = blocked(resume_with(
        &definition,
        continuation,
        Resume::Release(json!(null)),
        at(1_005),
    ));
    assert_eq!(second.execution_key().to_string(), "attempt:1");

    let (retry, continuation) = blocked(resume_with(
        &definition,
        _continuation,
        down.clone(),
        at(1_006),
    ));
    assert!(matches!(retry.pause(), Pause::Retry { attempt: 1, .. }));

    let (third, continuation) = blocked(resume_with(
        &definition,
        continuation,
        Resume::Release(json!(null)),
        at(1_009),
    ));
    assert_eq!(third.execution_key().to_string(), "attempt:2");

    let (output, _) = match resume_with(&definition, continuation, down, at(1_010)) {
        Outcome::Completed { output, context } => (output, context),
        other => panic!("expected the catch, got {other:?}"),
    };
    assert_eq!(output, json!({"ok": false}));
}

#[test]
fn a_bad_limit_duration_faults() {
    let definition = sample(
        r#"
  - charge:
      try:
        - boom:
            raise:
              error:
                type: https://example.test/down
                status: 500
                title: down
      catch:
        retry:
          limit:
            duration: later
"#,
    );
    let Outcome::Faulted { fault, .. } = start(&definition, &json!({})) else {
        panic!("expected a fault");
    };
    assert_eq!(fault.instance(), "/do/0/charge");
    assert!(fault.detail().unwrap_or("").contains("later"), "{fault}");
}
