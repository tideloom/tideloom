use std::time::Duration;
use std::time::Instant;

use serde_json::json;
use tideloom_core::Continuation;
use tideloom_core::Counter;
use tideloom_core::Definition;
use tideloom_core::JitterSample;
use tideloom_core::Outcome;
use tideloom_core::Pause;
use tideloom_core::Resume;
use tideloom_core::Timestamp;
use tideloom_core::WalkOptions;
use tideloom_core::resume;
use tideloom_core::resume_with;
use tideloom_core::start;
use tideloom_core::start_with;

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

fn retry_pause(tasks: &str) -> tideloom_core::Block {
    blocked(start(&sample(tasks), &json!({}))).0
}

fn down(position: &str) -> Resume {
    Resume::Fault(tideloom_core::Fault::runtime(position, "down"))
}

fn assert_retry(block: &tideloom_core::Block, attempt: u64, delay: Duration) {
    assert_eq!(block.pause(), Pause::Retry { attempt, delay });
    let started = Timestamp::from_millis(1_700_000_000_000);
    assert_eq!(
        block.pause().retry_at(started),
        Some(Timestamp::from_millis(
            1_700_000_000_000 + u64::try_from(delay.as_millis()).unwrap()
        ))
    );
}

#[test]
fn constant_backoff_waits_then_retries_the_next_attempt() {
    let definition = sample(
        r#"
  - charge:
      try:
        - callPay:
            call: http
            with:
              method: post
              endpoint: https://example.test/pay
      catch:
        retry:
          delay: PT3S
          backoff:
            constant: {}
          limit:
            attempt:
              count: 1
"#,
    );
    let (call, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(call.position(), "/do/0/charge/try/0/callPay");
    assert_eq!(call.execution_key().counters(), &[Counter::Attempt(0)]);

    let (retry, continuation) = blocked(resume(&definition, continuation, down(call.position())));
    assert_eq!(retry.position(), "/do/0/charge");
    assert!(!retry.effectful());
    assert_retry(&retry, 0, Duration::from_secs(3));
    assert_eq!(retry.execution_key().to_string(), "attempt:0");
    assert_eq!(continuation.pause(), retry.pause());

    let (again, continuation) = blocked(resume(&definition, continuation, Resume::Tick));
    assert_eq!(again.key(), retry.key());
    assert_retry(&again, 0, Duration::from_secs(3));

    let (retried, continuation) = blocked(resume(
        &definition,
        continuation,
        Resume::Release(json!(null)),
    ));
    assert_eq!(retried.position(), "/do/0/charge/try/0/callPay");
    assert_eq!(retried.execution_key().counters(), &[Counter::Attempt(1)]);

    let Outcome::Completed { output, .. } = resume(
        &definition,
        continuation,
        Resume::Output(json!({"paid": true})),
    ) else {
        panic!("expected completion");
    };
    assert_eq!(output, json!({"paid": true}));
}

#[test]
fn linear_and_exponential_series_release_until_the_catch() {
    let linear = sample(
        r#"
  - charge:
      try:
        - boom:
            raise:
              error:
                type: https://example.test/down
                status: 503
                title: down
      catch:
        retry:
          delay:
            seconds: 3
          backoff:
            linear: {}
          limit:
            attempt:
              count: 2
        do:
          - recover:
              set:
                ok: false
"#,
    );
    let (first, continuation) = blocked(start(&linear, &json!({})));
    assert_retry(&first, 0, Duration::from_secs(3));
    let (second, continuation) =
        blocked(resume(&linear, continuation, Resume::Release(json!(null))));
    assert_retry(&second, 1, Duration::from_secs(6));
    let (output, _) = match resume(&linear, continuation, Resume::Release(json!(null))) {
        Outcome::Completed { output, context } => (output, context),
        other => panic!("expected the catch, got {other:?}"),
    };
    assert_eq!(output, json!({"ok": false}));

    let exponential = sample(
        r#"
  - charge:
      try:
        - boom:
            raise:
              error:
                type: https://example.test/down
                status: 503
                title: down
      catch:
        retry:
          delay: PT1S
          backoff:
            exponential: {}
          limit:
            attempt:
              count: 3
"#,
    );
    let mut outcome = start(&exponential, &json!({}));
    let expected = [1_u64, 2, 4];
    for (attempt, seconds) in expected.into_iter().enumerate() {
        let (pause, continuation) = blocked(outcome);
        assert_retry(
            &pause,
            u64::try_from(attempt).unwrap(),
            Duration::from_secs(seconds),
        );
        outcome = resume(&exponential, continuation, Resume::Release(json!(null)));
    }
    let Outcome::Faulted { fault, .. } = outcome else {
        panic!("expected the raised fault after the last attempt");
    };
    assert_eq!(fault.instance(), "/do/0/charge/try/0/boom");
}

#[test]
fn jitter_is_added_after_backoff_and_stays_on_the_chosen_sample() {
    let definition = sample(
        r#"
  - charge:
      try:
        - boom:
            raise:
              error:
                type: https://example.test/down
                status: 503
                title: down
      catch:
        retry:
          delay:
            seconds: 1
          backoff:
            exponential: {}
          jitter:
            from:
              milliseconds: 100
            to:
              milliseconds: 500
          limit:
            attempt:
              count: 2
"#,
    );
    let (from_end, continuation) = blocked(start(&definition, &json!({})));
    assert_retry(&from_end, 0, Duration::from_millis(1_100));

    // The delay was fixed when the pause was created. Another sample on a
    // later tick does not move it.
    let (same, _) = blocked(resume_with(
        &definition,
        continuation,
        Resume::Tick,
        WalkOptions::new().with_jitter(JitterSample::TO),
    ));
    assert_eq!(same.pause(), from_end.pause());

    let (to_end, _) = blocked(start_with(
        &definition,
        &json!({}),
        WalkOptions::new().with_jitter(JitterSample::TO),
    ));
    assert_retry(&to_end, 0, Duration::from_millis(1_500));

    let mid = JitterSample::new(500_000).unwrap();
    let (middle, _) = blocked(start_with(
        &definition,
        &json!({}),
        WalkOptions::new().with_jitter(mid),
    ));
    assert_retry(&middle, 0, Duration::from_millis(1_300));

    let options = WalkOptions::new().with_jitter(JitterSample::FROM);
    let (first, continuation) = blocked(start_with(&definition, &json!({}), options));
    assert_retry(&first, 0, Duration::from_millis(1_100));
    let (second, _) = blocked(resume_with(
        &definition,
        continuation,
        Resume::Release(json!(null)),
        options,
    ));
    assert_retry(&second, 1, Duration::from_millis(2_100));
}

#[test]
fn durations_accept_iso_objects_expressions_and_named_policies() {
    let iso = retry_pause(
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
          delay: PT1.5S
"#,
    );
    assert_retry(&iso, 0, Duration::from_millis(1_500));

    let object = retry_pause(
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
          delay:
            minutes: 1
            seconds: 1
"#,
    );
    assert_retry(&object, 0, Duration::from_millis(61_000));

    let expressed = retry_pause(
        r#"
  - charge:
      try:
        - boom:
            raise:
              error:
                type: https://example.test/down
                status: 500
                title: down
                detail: PT2S
      catch:
        retry:
          delay: ${ $error.detail }
"#,
    );
    assert_retry(&expressed, 0, Duration::from_secs(2));

    let named = document(
        r#"
use:
  retries:
    patient:
      delay:
        milliseconds: 40
      backoff:
        linear:
          increment:
            milliseconds: 10
      limit:
        attempt:
          count: 2
"#,
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
        retry: patient
"#,
    );
    let (first, continuation) = blocked(start(&named, &json!({})));
    assert_retry(&first, 0, Duration::from_millis(40));
    let (second, _) = blocked(resume(&named, continuation, Resume::Release(json!(null))));
    assert_retry(&second, 1, Duration::from_millis(50));
}

#[test]
fn a_bad_policy_faults_instead_of_pausing() {
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
          delay: PT3S
          backoff:
            exponential:
              factor: 2
"#,
    );
    let Outcome::Faulted { fault, .. } = start(&definition, &json!({})) else {
        panic!("expected a fault");
    };
    assert_eq!(fault.instance(), "/do/0/charge");
    assert!(fault.detail().unwrap_or("").contains("factor"), "{fault}");

    let backwards = sample(
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
          jitter:
            from: PT2S
            to: PT1S
"#,
    );
    let Outcome::Faulted { fault, .. } = start(&backwards, &json!({})) else {
        panic!("expected a fault");
    };
    assert!(
        fault.detail().unwrap_or("").contains("jitter.from"),
        "{fault}"
    );
}

#[test]
fn a_for_loop_retries_under_that_iteration_attempt_key() {
    let definition = sample(
        r#"
  - each:
      for:
        each: item
        in: .items
      do:
        - charge:
            try:
              - callPay:
                  call: http
            catch:
              retry:
                delay:
                  seconds: 2
                backoff:
                  constant: {}
                limit:
                  attempt:
                    count: 1
"#,
    );
    let input = json!({"items": [1, 2]});
    let (call, continuation) = blocked(start(&definition, &input));
    assert_eq!(call.position(), "/do/0/each/do/0/charge/try/0/callPay");
    assert_eq!(
        call.execution_key().counters(),
        &[Counter::Loop(0), Counter::Attempt(0)]
    );

    let (retry, continuation) = blocked(resume(&definition, continuation, down(call.position())));
    assert_eq!(retry.position(), "/do/0/each/do/0/charge");
    assert_retry(&retry, 0, Duration::from_secs(2));
    assert_eq!(
        retry.execution_key().counters(),
        &[Counter::Loop(0), Counter::Attempt(0)]
    );

    let (retried, continuation) = blocked(resume(
        &definition,
        continuation,
        Resume::Release(json!(null)),
    ));
    assert_eq!(retried.position(), call.position());
    assert_eq!(
        retried.execution_key().counters(),
        &[Counter::Loop(0), Counter::Attempt(1)]
    );

    let (next, _) = blocked(resume(
        &definition,
        continuation,
        Resume::Output(json!({"paid": true})),
    ));
    assert_eq!(
        next.execution_key().counters(),
        &[Counter::Loop(1), Counter::Attempt(0)]
    );
}

#[test]
fn start_returns_the_retry_pause_without_sleeping() {
    let definition = sample(
        r#"
  - charge:
      try:
        - boom:
            raise:
              error:
                type: https://example.test/down
                status: 503
                title: down
      catch:
        retry:
          delay:
            seconds: 30
          backoff:
            constant: {}
"#,
    );
    let started = Instant::now();
    let (pause, _) = blocked(start(&definition, &json!({})));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_retry(&pause, 0, Duration::from_secs(30));
}
