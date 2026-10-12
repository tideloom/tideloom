use std::time::Duration;

use serde_json::json;
use tideloom_core::Continuation;
use tideloom_core::Counter;
use tideloom_core::Definition;
use tideloom_core::NodeKind;
use tideloom_core::Outcome;
use tideloom_core::Pause;
use tideloom_core::Resume;
use tideloom_core::RunKind;
use tideloom_core::resume;
use tideloom_core::start;

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

fn completed(outcome: Outcome) -> (serde_json::Value, serde_json::Value) {
    match outcome {
        Outcome::Completed { output, context } => (output, context),
        other => panic!("expected completion, got {other:?}"),
    }
}

fn faulted(outcome: Outcome) -> tideloom_core::Fault {
    match outcome {
        Outcome::Faulted { fault, .. } => fault,
        other => panic!("expected a fault, got {other:?}"),
    }
}

#[test]
fn sets_run_inline_to_completion() {
    let definition = sample(
        r#"
  - paint:
      set:
        color: red
  - seal:
      set:
        sealed: true
"#,
    );
    let (output, _) = completed(start(&definition, &json!({})));
    assert_eq!(output, json!({"color": "red", "sealed": true}));
}

#[test]
fn stops_at_the_first_effectful_block_after_inline_work() {
    let definition = sample(
        r#"
  - paint:
      set:
        color: red
  - notify:
      emit: {}
"#,
    );
    let before = definition.clone();
    let (block, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(definition, before);
    assert_eq!(block.position(), "/do/1/notify");
    assert_eq!(block.name(), Some("notify"));
    assert!(block.effectful());
    assert!(block.execution_key().is_empty());
    assert_eq!(block.pause(), Pause::Activity);
    assert!(matches!(block.kind(), NodeKind::Emit));
    assert_eq!(block.input(), &json!({"color": "red"}));
    assert_eq!(continuation.position(), "/do/1/notify");
    assert_eq!(continuation.pause(), Pause::Activity);
    assert_eq!(&continuation.key(), block.key());
    assert_eq!(continuation.to_string(), "/do/1/notify");
    assert_eq!(continuation.depth(), 1);
}

#[test]
fn an_output_resumes_the_tasks_after_the_block() {
    let definition = sample(
        r#"
  - paint:
      set:
        color: red
  - notify:
      emit: {}
  - seal:
      set:
        sealed: true
"#,
    );
    let (first, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(first.position(), "/do/1/notify");
    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Output(json!({"sent": true})),
    ));
    assert_eq!(output, json!({"sent": true, "sealed": true}));
}

#[test]
fn effectful_tasks_stop_one_at_a_time() {
    let definition = sample(
        r#"
  - first:
      emit: {}
  - second:
      emit: {}
"#,
    );
    let (first, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(first.position(), "/do/0/first");
    let (second, continuation) = blocked(resume(
        &definition,
        continuation,
        Resume::Output(json!({"n": 1})),
    ));
    assert_eq!(second.position(), "/do/1/second");
    assert_eq!(second.input(), &json!({"n": 1}));
    assert_ne!(first.key(), second.key());
    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Output(json!({"n": 2})),
    ));
    assert_eq!(output, json!({"n": 2}));
}

#[test]
fn the_same_start_is_deterministic() {
    let definition = sample(
        r#"
  - paint:
      set:
        color: red
  - notify:
      emit: {}
"#,
    );
    assert_eq!(
        start(&definition, &json!({"n": 1})),
        start(&definition, &json!({"n": 1}))
    );
}

#[test]
fn a_continuation_round_trips_through_json() {
    let definition = sample(
        r#"
  - fan:
      for:
        each: item
        in: .items
      do:
        - charge:
            try:
              - callPay:
                  call: http
            catch:
              do:
                - recover:
                    set:
                      paid: false
  - seal:
      set:
        sealed: true
"#,
    );
    let input = json!({"items": ["a"]});
    let (block, continuation) = blocked(start(&definition, &input));
    assert_eq!(
        continuation.to_string(),
        "/do/0/fan/do/0/charge/try/0/callPay@loop:0/attempt:0"
    );
    assert_eq!(continuation.depth(), 5);
    let stored = serde_json::to_string(&continuation).unwrap_or_else(|error| panic!("{error}"));
    let restored: Continuation =
        serde_json::from_str(&stored).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(restored, continuation);
    assert_eq!(&restored.key(), block.key());
    let (output, _) = completed(resume(
        &definition,
        restored,
        Resume::Output(json!({"paid": true})),
    ));
    assert_eq!(output, json!({"paid": true, "sealed": true}));
}

#[test]
fn a_tick_without_a_deadline_returns_the_same_block() {
    let definition = sample(
        r#"
  - notify:
      emit: {}
"#,
    );
    let (block, continuation) = blocked(start(&definition, &json!({})));
    let (again, same) = blocked(resume(&definition, continuation.clone(), Resume::Tick));
    assert_eq!(again, block);
    assert_eq!(same, continuation);
}

#[test]
fn a_blocking_pause_rejects_an_output() {
    let definition = sample(
        r#"
  - pause:
      wait: {}
"#,
    );
    let (_, continuation) = blocked(start(&definition, &json!({})));
    let fault = faulted(resume(&definition, continuation, Resume::Output(json!({}))));
    assert_eq!(fault.instance(), "/do/0/pause");
    assert!(fault.detail().unwrap_or("").contains("Release"), "{fault}");
}

#[test]
fn wait_is_a_timer_until_it_is_released() {
    let definition = sample(
        r#"
  - pause:
      wait: {}
  - seal:
      set:
        sealed: true
"#,
    );
    let (block, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(block.position(), "/do/0/pause");
    assert!(!block.effectful());
    assert_eq!(block.pause(), Pause::Timer);
    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Release(json!({"held": true})),
    ));
    assert_eq!(output, json!({"held": true, "sealed": true}));
}

#[test]
fn for_iterations_use_distinct_execution_keys() {
    let definition = sample(
        r#"
  - fan:
      for:
        each: item
        in: .items
      do:
        - ping:
            emit: {}
"#,
    );
    let input = json!({"items": ["a", "b"]});
    let (first, continuation) = blocked(start(&definition, &input));
    assert_eq!(first.position(), "/do/0/fan/do/0/ping");
    assert_eq!(first.execution_key().counters(), &[Counter::Loop(0)]);
    assert_eq!(first.execution_key().to_string(), "loop:0");
    assert_eq!(first.input(), &input);

    let (second, continuation) = blocked(resume(
        &definition,
        continuation,
        Resume::Output(json!({"sent": 0})),
    ));
    assert_eq!(second.execution_key().counters(), &[Counter::Loop(1)]);
    assert_eq!(second.input(), &json!({"sent": 0}));

    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Output(json!({"sent": 1})),
    ));
    assert_eq!(output, json!({"sent": 1}));
}

#[test]
fn loop_variables_are_bound_after_a_resume() {
    let definition = sample(
        r#"
  - fan:
      for:
        each: item
        in: .items
      do:
        - ping:
            emit: {}
        - mark:
            set:
              seen: ${ $item }
              at: ${ $index }
"#,
    );
    let input = json!({"items": ["a", "b"]});
    let (_, continuation) = blocked(start(&definition, &input));
    let (second, continuation) =
        blocked(resume(&definition, continuation, Resume::Output(json!({}))));
    assert_eq!(second.input(), &json!({"seen": "a", "at": 0}));
    let (output, _) = completed(resume(&definition, continuation, Resume::Output(json!({}))));
    assert_eq!(output, json!({"seen": "b", "at": 1}));
}

#[test]
fn while_stops_the_loop_before_later_iterations() {
    let definition = sample(
        r#"
  - fan:
      for:
        each: item
        in: .items
      while: $index < 1
      do:
        - mark:
            set:
              n: ${ $item }
"#,
    );
    let (output, _) = completed(start(&definition, &json!({"items": ["a", "b", "c"]})));
    assert_eq!(output["n"], json!("a"));
}

#[test]
fn retry_attempts_are_part_of_the_effectful_key() {
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
          limit:
            attempt:
              count: 1
        do:
          - recover:
              set:
                paid: false
"#,
    );
    let (call, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(call.position(), "/do/0/charge/try/0/callPay");
    assert!(call.effectful());
    assert_eq!(call.pause(), Pause::Activity);
    assert_eq!(call.execution_key().counters(), &[Counter::Attempt(0)]);
    assert_eq!(call.execution_key().to_string(), "attempt:0");

    let (retry, continuation) = blocked(resume(
        &definition,
        continuation,
        Resume::Fault(tideloom_core::Fault::runtime(call.position(), "down")),
    ));
    assert_eq!(retry.position(), "/do/0/charge");
    assert!(!retry.effectful());
    assert_eq!(
        retry.pause(),
        Pause::Retry {
            attempt: 0,
            delay: Duration::ZERO,
        }
    );

    let (again, continuation) = blocked(resume(
        &definition,
        continuation,
        Resume::Release(json!(null)),
    ));
    assert_eq!(again.position(), "/do/0/charge/try/0/callPay");
    assert_eq!(again.execution_key().counters(), &[Counter::Attempt(1)]);
    assert_ne!(call.key(), again.key());

    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Output(json!({"paid": true})),
    ));
    assert_eq!(output, json!({"paid": true}));
}

#[test]
fn raise_inside_try_is_caught_inline() {
    let definition = sample(
        r#"
  - attempt:
      try:
        - boom:
            raise:
              error:
                type: https://open-workflow-specification.org/spec/1.0.0/errors/runtime
                status: 500
                title: nope
      catch:
        do:
          - recover:
              set:
                ok: true
"#,
    );
    let (output, _) = completed(start(&definition, &json!({})));
    assert_eq!(output, json!({"ok": true}));
}

#[test]
fn an_effectful_fault_is_caught_without_stopping_again() {
    let definition = sample(
        r#"
  - charge:
      try:
        - callPay:
            call: http
      catch:
        do:
          - recover:
              set:
                paid: false
"#,
    );
    let (call, continuation) = blocked(start(&definition, &json!({})));
    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Fault(tideloom_core::Fault::runtime(call.position(), "down")),
    ));
    assert_eq!(output, json!({"paid": false}));
}

#[test]
fn the_error_binding_is_visible_after_a_resume_inside_the_catch() {
    let definition = sample(
        r#"
  - charge:
      try:
        - callPay:
            call: http
      catch:
        as: problem
        do:
          - notify:
              emit: {}
          - recover:
              set:
                status: ${ $problem.status }
"#,
    );
    let (call, continuation) = blocked(start(&definition, &json!({})));
    let (notify, continuation) = blocked(resume(
        &definition,
        continuation,
        Resume::Fault(tideloom_core::Fault::runtime(call.position(), "down")),
    ));
    assert_eq!(notify.position(), "/do/0/charge/catch/do/0/notify");
    assert_eq!(notify.execution_key().to_string(), "attempt:0");
    let (output, _) = completed(resume(&definition, continuation, Resume::Output(json!({}))));
    assert_eq!(output, json!({"status": 500}));
}

#[test]
fn uncaught_raise_faults_at_the_task_pointer() {
    let definition = sample(
        r#"
  - boom:
      raise:
        error:
          type: https://open-workflow-specification.org/spec/1.0.0/errors/runtime
          status: 500
          title: nope
"#,
    );
    let fault = faulted(start(&definition, &json!({})));
    assert_eq!(fault.instance(), "/do/0/boom");
    assert_eq!(fault.status(), 500);
    assert_eq!(fault.title(), "nope");
}

#[test]
fn switch_jumps_to_the_matching_sibling() {
    let definition = sample(
        r#"
  - route:
      switch:
        - high:
            when: .priority == "high"
            then: rush
        - other:
            then: slow
  - rush:
      set:
        lane: rush
      then: end
  - slow:
      set:
        lane: slow
      then: end
"#,
    );
    let (high, _) = completed(start(&definition, &json!({"priority": "high"})));
    assert_eq!(high["lane"], json!("rush"));
    let (low, _) = completed(start(&definition, &json!({"priority": "low"})));
    assert_eq!(low["lane"], json!("slow"));
}

#[test]
fn a_jump_back_through_an_effectful_task_gets_a_new_pass_key() {
    let definition = sample(
        r#"
  - poll:
      call: http
      with:
        method: get
        endpoint: https://example.test/job/1
  - check:
      switch:
        - pending:
            when: .status == "pending"
            then: poll
        - done:
            then: end
"#,
    );
    let (first, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(first.position(), "/do/0/poll");
    assert!(first.execution_key().is_empty());

    let (second, continuation) = blocked(resume(
        &definition,
        continuation,
        Resume::Output(json!({"status": "pending"})),
    ));
    assert_eq!(second.position(), "/do/0/poll");
    assert_eq!(second.execution_key().counters(), &[Counter::Pass(1)]);
    assert_eq!(second.key().to_string(), "/do/0/poll@pass:1");
    assert_eq!(second.input(), &json!({"status": "pending"}));

    let (third, continuation) = blocked(resume(
        &definition,
        continuation,
        Resume::Output(json!({"status": "pending"})),
    ));
    assert_eq!(third.execution_key().counters(), &[Counter::Pass(2)]);

    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Output(json!({"status": "done"})),
    ));
    assert_eq!(output, json!({"status": "done"}));
}

#[test]
fn end_completes_before_later_siblings() {
    let definition = sample(
        r#"
  - first:
      set:
        n: 1
      then: end
  - second:
      set:
        n: 2
"#,
    );
    let (output, _) = completed(start(&definition, &json!({})));
    assert_eq!(output, json!({"n": 1}));
}

#[test]
fn end_after_a_resume_skips_the_rest_of_every_list() {
    let definition = sample(
        r#"
  - outer:
      do:
        - notify:
            emit: {}
            then: end
        - skipped:
            set:
              n: 2
  - after:
      set:
        tail: true
"#,
    );
    let (_, continuation) = blocked(start(&definition, &json!({})));
    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Output(json!({"n": 1})),
    ));
    assert_eq!(output, json!({"n": 1}));
}

#[test]
fn exit_leaves_the_nested_do() {
    let definition = sample(
        r#"
  - outer:
      do:
        - inner:
            do:
              - leave:
                  set:
                    n: 1
                  then: exit
              - skipped:
                  set:
                    n: 2
        - after:
            set:
              tail: true
"#,
    );
    let (output, _) = completed(start(&definition, &json!({})));
    assert_eq!(output, json!({"n": 1, "tail": true}));
}

#[test]
fn a_local_function_runs_inline() {
    let definition = document(
        r#"
use:
  functions:
    normalize:
      set:
        ok: true
"#,
        r#"
  - go:
      call: normalize
"#,
    );
    let (output, _) = completed(start(&definition, &json!({"n": 1})));
    assert_eq!(output, json!({"n": 1, "ok": true}));
}

#[test]
fn an_undefined_function_faults_instead_of_calling_out() {
    let definition = sample(
        r#"
  - go:
      call: missing
"#,
    );
    let fault = faulted(start(&definition, &json!({})));
    assert_eq!(fault.instance(), "/do/0/go");
    assert!(fault.detail().unwrap_or("").contains("missing"), "{fault}");
}

#[test]
fn a_function_body_stops_at_its_own_effectful_node() {
    let definition = document(
        r#"
use:
  functions:
    ping:
      emit: {}
"#,
        r#"
  - go:
      call: ping
      output:
        as: .pinged
  - after:
      set:
        tail: true
"#,
    );
    let (block, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(block.position(), "/do/0/go/_fn");
    assert!(block.effectful());
    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Output(json!({"pinged": {"ok": true}})),
    ));
    assert_eq!(output, json!({"ok": true, "tail": true}));
}

#[test]
fn fork_stops_for_the_join_and_does_not_run_branches() {
    let definition = sample(
        r#"
  - prep:
      set:
        ready: true
  - rush:
      fork:
        compete: true
        branches:
          - ship:
              set:
                lane: ship
  - after:
      set:
        joined: true
"#,
    );
    let (block, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(block.position(), "/do/1/rush");
    assert!(!block.effectful());
    assert_eq!(block.pause(), Pause::Join { compete: true });
    assert_eq!(block.input(), &json!({"ready": true}));
    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Release(json!({"winner": "ship"})),
    ));
    assert_eq!(output, json!({"winner": "ship", "joined": true}));
}

#[test]
fn listen_is_an_event_pause_and_foreach_is_not_walked() {
    let definition = sample(
        r#"
  - hear:
      listen:
        to:
          one:
            with:
              type: io.example.paid
      foreach:
        do:
          - mark:
              set:
                marked: true
  - after:
      set:
        done: true
"#,
    );
    let (block, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(block.pause(), Pause::Events);
    assert!(!block.effectful());
    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Release(json!({"events": 1})),
    ));
    assert_eq!(output, json!({"events": 1, "done": true}));
}

#[test]
fn run_workflow_is_an_effectful_child_pause() {
    let definition = sample(
        r#"
  - child:
      run:
        workflow:
          namespace: orders
          name: receipt
          version: '1.0.0'
"#,
    );
    let (block, _) = blocked(start(&definition, &json!({})));
    assert!(block.effectful());
    assert_eq!(block.pause(), Pause::Child);
    assert!(matches!(block.kind(), NodeKind::Run(RunKind::Workflow)));
}

#[test]
fn http_call_stops_without_being_executed() {
    let definition = sample(
        r#"
  - callPay:
      call: http
      with:
        method: get
        endpoint: https://example.test/pay
"#,
    );
    let (block, _) = blocked(start(&definition, &json!({})));
    assert_eq!(block.position(), "/do/0/callPay");
    assert_eq!(block.pause(), Pause::Activity);
    assert!(block.effectful());
}

#[test]
fn if_false_skips_an_effectful_task() {
    let definition = sample(
        r#"
  - maybe:
      if: .go
      emit: {}
  - after:
      set:
        ran: true
"#,
    );
    let (output, _) = completed(start(&definition, &json!({"go": false})));
    assert_eq!(output, json!({"go": false, "ran": true}));
    let (block, _) = blocked(start(&definition, &json!({"go": true})));
    assert_eq!(block.position(), "/do/0/maybe");
}

#[test]
fn output_as_is_applied_to_an_activity_output() {
    let definition = sample(
        r#"
  - notify:
      emit: {}
      output:
        as: .sent
"#,
    );
    let (_, continuation) = blocked(start(&definition, &json!({})));
    let (output, _) = completed(resume(
        &definition,
        continuation,
        Resume::Output(json!({"sent": true, "other": 1})),
    ));
    assert_eq!(output, json!(true));
}

#[test]
fn export_as_is_visible_to_later_tasks() {
    let definition = sample(
        r#"
  - remember:
      set:
        color: red
      export:
        as: '{ color: .color }'
  - later:
      set:
        seen: ${ $context.color }
"#,
    );
    let (output, context) = completed(start(&definition, &json!({})));
    assert_eq!(output, json!({"color": "red", "seen": "red"}));
    assert_eq!(context, json!({"color": "red"}));
}

#[test]
fn context_survives_a_stop() {
    let definition = sample(
        r#"
  - remember:
      set:
        color: red
      export:
        as: '{ color: .color }'
  - notify:
      emit: {}
  - later:
      set:
        seen: ${ $context.color }
"#,
    );
    let (block, continuation) = blocked(start(&definition, &json!({})));
    assert_eq!(block.context(), &json!({"color": "red"}));
    assert_eq!(continuation.context(), json!({"color": "red"}));
    let (output, context) = completed(resume(&definition, continuation, Resume::Output(json!({}))));
    assert_eq!(output, json!({"seen": "red"}));
    assert_eq!(context, json!({"color": "red"}));
}

#[test]
fn workflow_and_task_input_from_transform_data() {
    let definition = document(
        "input:\n  from: .order\n",
        r#"
  - show:
      set:
        seen: ${ .id }
"#,
    );
    let (output, _) = completed(start(
        &definition,
        &json!({"order": {"id": 7}, "extra": true}),
    ));
    assert_eq!(output, json!({"id": 7, "seen": 7}));

    let definition = sample(
        r#"
  - take:
      input:
        from: .inner
      set:
        n: 1
"#,
    );
    let (output, _) = completed(start(&definition, &json!({"inner": {"a": 1}, "b": 2})));
    assert_eq!(output, json!({"a": 1, "n": 1}));
}

#[test]
fn an_unsupported_expression_is_an_expression_fault() {
    let definition = sample(
        r#"
  - check:
      if: map(.)
      set:
        n: 1
"#,
    );
    let fault = faulted(start(&definition, &json!({})));
    assert_eq!(fault.instance(), "/do/0/check");
    assert_eq!(fault.status(), 400);
}

#[test]
fn a_missing_sibling_is_a_runtime_fault() {
    let definition = sample(
        r#"
  - first:
      set:
        n: 1
      then: missing
"#,
    );
    let fault = faulted(start(&definition, &json!({})));
    assert_eq!(fault.instance(), "/do/0/first");
    assert_eq!(fault.status(), 500);
}

#[test]
fn a_named_retry_is_loaded_from_use() {
    let definition = document(
        r#"
use:
  retries:
    once:
      limit:
        attempt:
          count: 1
"#,
        r#"
  - charge:
      try:
        - callPay:
            call: http
      catch:
        retry: once
"#,
    );
    let (call, continuation) = blocked(start(&definition, &json!({})));
    let (retry, _) = blocked(resume(
        &definition,
        continuation,
        Resume::Fault(tideloom_core::Fault::runtime(call.position(), "down")),
    ));
    assert_eq!(
        retry.pause(),
        Pause::Retry {
            attempt: 0,
            delay: Duration::ZERO,
        }
    );
    assert_eq!(retry.execution_key().to_string(), "attempt:0");
}

#[test]
fn catch_errors_with_can_decline_a_fault() {
    let definition = sample(
        r#"
  - attempt:
      try:
        - boom:
            raise:
              error:
                type: https://example.test/other
                status: 500
                title: nope
      catch:
        errors:
          with:
            type: https://example.test/wanted
        do:
          - recover:
              set:
                ok: true
"#,
    );
    let fault = faulted(start(&definition, &json!({})));
    assert_eq!(fault.error_type(), "https://example.test/other");
    assert_eq!(fault.instance(), "/do/0/attempt/try/0/boom");
}

#[test]
fn an_inline_cycle_faults_instead_of_looping_forever() {
    let definition = sample(
        r#"
  - again:
      set:
        n: 1
      then: again
"#,
    );
    let fault = faulted(start(&definition, &json!({})));
    assert!(fault.detail().unwrap_or("").contains("10000"), "{fault}");
}
