use serde_json::json;
use tideloom_core::Definition;
use tideloom_core::Outcome;
use tideloom_core::Pause;
use tideloom_core::ResultLog;
use tideloom_core::Timestamp;
use tideloom_core::WalkOptions;
use tideloom_core::walk;
use tideloom_core::walk_with;

fn sample(tasks: &str) -> Definition {
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: sample\n  version: '0.1.0'\ndo:\n{tasks}"
    );
    Definition::from_yaml(&yaml).unwrap_or_else(|error| panic!("{error}"))
}

fn blocked(outcome: Outcome) -> tideloom_core::Block {
    match outcome {
        Outcome::Blocked { block } => block,
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
    let mut log = ResultLog::new();
    let call = blocked(walk(&definition, &json!({}), &log));
    let started = Timestamp::from_millis(100);
    log.start_task(call.key().clone(), started);

    let still = blocked(walk_with(&definition, &json!({}), &log, at(104)));
    assert_eq!(still.key(), call.key());

    let retry = blocked(walk_with(&definition, &json!({}), &log, at(105)));
    assert_eq!(retry.position(), "/do/0/charge");
    assert_eq!(
        retry.pause(),
        Pause::Retry {
            attempt: 0,
            delay: std::time::Duration::from_secs(30),
        }
    );
    log.release(retry.key().clone(), json!(null));

    let again = blocked(walk_with(&definition, &json!({}), &log, at(105)));
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
    let mut log = ResultLog::new();
    let call = blocked(walk(&definition, &json!({}), &log));
    log.record_fault(
        call.key().clone(),
        tideloom_core::Fault::runtime(call.position(), "down"),
    );
    let retry = blocked(walk(&definition, &json!({}), &log));
    assert!(matches!(retry.pause(), Pause::Retry { attempt: 0, .. }));
    let started = Timestamp::from_millis(1_000);
    log.start_task(retry.key().clone(), started);

    let waiting = blocked(walk_with(&definition, &json!({}), &log, at(1_009)));
    assert_eq!(waiting.key(), retry.key());

    let (output, _) = match walk_with(&definition, &json!({}), &log, at(1_010)) {
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
    let Outcome::Faulted { fault, .. } = walk(&definition, &json!({}), &ResultLog::new()) else {
        panic!("expected a fault");
    };
    assert_eq!(fault.instance(), "/do/0/charge");
    assert!(fault.detail().unwrap_or("").contains("later"), "{fault}");
}
