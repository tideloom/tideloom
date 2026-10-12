//! Block detail and `start_at`: what a runner reads off a block, and running
//! one task as its own root.

use std::time::Duration;

use serde_json::json;
use tideloom_core::BlockDetail;
use tideloom_core::Continuation;
use tideloom_core::Definition;
use tideloom_core::ListenStrategy;
use tideloom_core::Outcome;
use tideloom_core::Pause;
use tideloom_core::Resume;
use tideloom_core::SCHEMA;
use tideloom_core::resume;
use tideloom_core::start;
use tideloom_core::start_at;

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

fn faulted(outcome: Outcome) -> tideloom_core::Fault {
    match outcome {
        Outcome::Faulted { fault, .. } => fault,
        other => panic!("expected a fault, got {other:?}"),
    }
}

#[test]
fn wait_detail_is_the_duration_and_empty_wait_is_zero() {
    let timed = sample(
        r#"
  - pause:
      wait:
        seconds: 90
"#,
    );
    let (block, _) = blocked(start(&timed, &json!({})));
    assert_eq!(
        block.detail(),
        &BlockDetail::Wait {
            after: Duration::from_secs(90)
        }
    );

    let iso = sample(
        r#"
  - pause:
      wait: PT1.5S
"#,
    );
    let (block, _) = blocked(start(&iso, &json!({})));
    assert_eq!(
        block.detail(),
        &BlockDetail::Wait {
            after: Duration::from_millis(1_500)
        }
    );

    let empty = sample(
        r#"
  - pause:
      wait: {}
"#,
    );
    let (block, _) = blocked(start(&empty, &json!({})));
    assert_eq!(
        block.detail(),
        &BlockDetail::Wait {
            after: Duration::ZERO
        }
    );
}

#[test]
fn activity_and_retry_blocks_carry_no_detail() {
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
              count: 1
"#,
    );
    let (call, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(call.detail(), &BlockDetail::None);
    let (retry, _) = blocked(resume(
        &definition,
        continuation,
        Resume::Fault(tideloom_core::Fault::runtime(call.position(), "down")),
    ));
    assert!(matches!(retry.pause(), Pause::Retry { .. }));
    assert_eq!(retry.detail(), &BlockDetail::None);
}

#[test]
fn fork_detail_lists_the_branches_in_order() {
    let definition = sample(
        r#"
  - race:
      fork:
        compete: true
        branches:
          - ship:
              set:
                lane: ship
          - fly:
              set:
                lane: fly
"#,
    );
    let (block, _) = blocked(start(&definition, &json!({})));
    let BlockDetail::Branches(branches) = block.detail() else {
        panic!("expected branches, got {:?}", block.detail());
    };
    let listed: Vec<(&str, &str)> = branches
        .iter()
        .map(|branch| (branch.position.as_str(), branch.name.as_str()))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("/do/0/race/fork/branches/0/ship", "ship"),
            ("/do/0/race/fork/branches/1/fly", "fly"),
        ]
    );
}

#[test]
fn start_at_runs_a_branch_as_its_own_root() {
    let definition = sample(
        r#"
  - race:
      fork:
        branches:
          - ship:
              do:
                - notify:
                    emit: {}
                - mark:
                    set:
                      lane: ship
          - fly:
              set:
                lane: fly
"#,
    );
    let (block, continuation) = blocked(start_at(
        &definition,
        "/do/0/race/fork/branches/0/ship",
        &json!({"order": 1}),
    ));
    assert_eq!(
        block.position(),
        "/do/0/race/fork/branches/0/ship/do/0/notify"
    );
    assert_eq!(block.input(), &json!({"order": 1}));
    assert_eq!(
        continuation.root_task(),
        Some("/do/0/race/fork/branches/0/ship")
    );
    assert_eq!(continuation.schema(), SCHEMA);
    let Outcome::Completed { output, .. } = resume(
        &definition,
        continuation,
        Resume::Output(json!({"sent": true})),
    ) else {
        panic!("expected the branch to complete");
    };
    assert_eq!(output, json!({"sent": true, "lane": "ship"}));

    let Outcome::Completed { output, .. } = start_at(
        &definition,
        "/do/0/race/fork/branches/1/fly",
        &json!({"order": 2}),
    ) else {
        panic!("expected the inline branch to complete");
    };
    assert_eq!(output, json!({"order": 2, "lane": "fly"}));
}

#[test]
fn start_at_rejects_an_unknown_position_and_a_sibling_jump() {
    let definition = sample(
        r#"
  - first:
      set:
        n: 1
      then: second
  - second:
      set:
        n: 2
"#,
    );
    let fault = faulted(start_at(&definition, "/do/9/missing", &json!({})));
    assert_eq!(fault.instance(), "/do/9/missing");

    let fault = faulted(start_at(&definition, "/do/0/first", &json!({})));
    assert_eq!(fault.instance(), "/do/0/first");
    assert!(fault.detail().unwrap_or("").contains("second"), "{fault}");
}

#[test]
fn a_root_task_fault_is_the_run_fault() {
    let definition = sample(
        r#"
  - boom:
      raise:
        error:
          type: https://example.test/down
          status: 503
          title: down
"#,
    );
    let fault = faulted(start_at(&definition, "/do/0/boom", &json!({})));
    assert_eq!(fault.status(), 503);
}

#[test]
fn child_detail_evaluates_the_input_and_reads_await() {
    let definition = sample(
        r#"
  - receipt:
      run:
        workflow:
          namespace: orders
          name: receipt
          version: '1.0.0'
          input:
            orderId: ${ .id }
        await: false
"#,
    );
    let (block, _) = blocked(start(&definition, &json!({"id": 7, "extra": true})));
    let BlockDetail::Child(child) = block.detail() else {
        panic!("expected a child, got {:?}", block.detail());
    };
    assert_eq!(child.namespace, "orders");
    assert_eq!(child.name, "receipt");
    assert_eq!(child.version, "1.0.0");
    assert_eq!(child.input, json!({"orderId": 7}));
    assert!(!child.await_result);

    let defaults = sample(
        r#"
  - receipt:
      run:
        workflow:
          namespace: orders
          name: receipt
          version: '1.0.0'
"#,
    );
    let (block, _) = blocked(start(&defaults, &json!({"id": 7})));
    let BlockDetail::Child(child) = block.detail() else {
        panic!("expected a child");
    };
    assert_eq!(child.input, json!({"id": 7}));
    assert!(child.await_result);
}

#[test]
fn listen_detail_keeps_static_filters() {
    let definition = sample(
        r#"
  - hear:
      listen:
        read: envelope
        to:
          all:
            - with:
                type: io.example.paid
                source: https://example.test/pay
            - with:
                type: io.example.shipped
"#,
    );
    let (block, _) = blocked(start(&definition, &json!({})));
    let BlockDetail::Listen(spec) = block.detail() else {
        panic!("expected a listen spec, got {:?}", block.detail());
    };
    assert_eq!(spec.strategy, ListenStrategy::All);
    assert_eq!(spec.read, "envelope");
    assert_eq!(spec.filters.len(), 2);
    assert_eq!(
        spec.filters[0].attributes.get("source").map(String::as_str),
        Some("https://example.test/pay")
    );
    assert_eq!(
        spec.filters[1].attributes.get("type").map(String::as_str),
        Some("io.example.shipped")
    );

    let one = sample(
        r#"
  - hear:
      listen:
        to:
          one:
            with:
              type: io.example.paid
"#,
    );
    let (block, _) = blocked(start(&one, &json!({})));
    let BlockDetail::Listen(spec) = block.detail() else {
        panic!("expected a listen spec");
    };
    assert_eq!(spec.strategy, ListenStrategy::One);
    assert_eq!(spec.read, "data");
}

#[test]
fn listen_rejects_expressions_correlation_and_until() {
    let expression = sample(
        r#"
  - hear:
      listen:
        to:
          one:
            with:
              type: ${ .kind }
"#,
    );
    let fault = faulted(start(&expression, &json!({"kind": "x"})));
    assert_eq!(fault.status(), 400);
    assert!(
        fault.detail().unwrap_or("").contains("expression"),
        "{fault}"
    );

    let correlate = sample(
        r#"
  - hear:
      listen:
        to:
          any:
            - with:
                type: io.example.paid
              correlate:
                orderId:
                  from: .data.orderId
"#,
    );
    let fault = faulted(start(&correlate, &json!({})));
    assert!(
        fault.detail().unwrap_or("").contains("correlate"),
        "{fault}"
    );

    let until = sample(
        r#"
  - hear:
      listen:
        to:
          any:
            - with:
                type: io.example.paid
          until: ${ true }
"#,
    );
    let fault = faulted(start(&until, &json!({})));
    assert!(fault.detail().unwrap_or("").contains("until"), "{fault}");
}

#[test]
fn an_unsupported_schema_is_refused() {
    let definition = sample(
        r#"
  - notify:
      emit: {}
"#,
    );
    let (_, continuation) = blocked(start(&definition, &json!({})));
    let mut stored = serde_json::to_value(&continuation).unwrap_or_else(|error| panic!("{error}"));
    stored["schema"] = json!(SCHEMA + 1);
    let newer: Continuation =
        serde_json::from_value(stored).unwrap_or_else(|error| panic!("{error}"));
    let fault = faulted(resume(&definition, newer, Resume::Output(json!({}))));
    assert_eq!(fault.status(), 400);
    assert!(fault.detail().unwrap_or("").contains("schema"), "{fault}");
}
