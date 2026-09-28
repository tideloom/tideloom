use std::collections::HashSet;

use tideloom_core::BuildError;
use tideloom_core::CallKind;
use tideloom_core::Definition;
use tideloom_core::FlowDirective;
use tideloom_core::Node;
use tideloom_core::NodeKind;
use tideloom_core::RunKind;
use tideloom_core::canonical_json;

fn flags(node: &Node, composite: bool, blocking: bool, effectful: bool) {
    assert_eq!(node.composite(), composite, "{}", node.position());
    assert_eq!(node.blocking(), blocking, "{}", node.position());
    assert_eq!(node.effectful(), effectful, "{}", node.position());
    assert_eq!(
        node.is_block_boundary(),
        blocking || effectful,
        "{}",
        node.position()
    );
    assert_eq!(
        node.records_task_execution(),
        effectful,
        "{}",
        node.position()
    );
}

#[test]
fn builds_each_task_kind_into_an_immutable_tree() {
    let yaml = r#"
document:
  dsl: '1.0.3'
  namespace: orders
  name: checkout
  version: '1.2.3'
  title: Checkout
use:
  functions:
    normalize:
      set:
        ok: true
do:
  - validateOrder:
      call: http
      with:
        method: post
        endpoint: https://example.test/validate
  - normalizeOrder:
      call: normalize
  - route:
      switch:
        - high:
            when: '.priority == "high"'
            then: rush
        - default:
            then: continue
      then: rush
  - rush:
      fork:
        compete: true
        branches:
          - ship:
              run:
                shell:
                  command: echo ship
          - note:
              emit:
                event:
                  with:
                    source: https://example.test/orders
                    type: io.example.noted
  - waitForPay:
      wait:
        seconds: 5
  - listenPaid:
      listen:
        to:
          one:
            with:
              type: io.example.paid
      foreach:
        do:
          - record:
              set:
                paid: true
  - charge:
      try:
        - callPay:
            call: grpc
      catch:
        retry:
          delay:
            seconds: 1
        do:
          - fallback:
              raise:
                error:
                  type: https://open-workflow-specification.org/spec/1.0.0/errors/runtime
                  status: 500
  - fanout:
      for:
        each: item
        in: .items
      do:
        - mark:
            set:
              seen: true
  - child:
      run:
        workflow:
          namespace: orders
          name: receipt
          version: '1.0.0'
  - scripted:
      run:
        script:
          language: js
          code: '1'
  - boxed:
      run:
        container:
          image: alpine
  - open:
      call: openapi
  - bus:
      call: asyncapi
  - agent:
      call: a2a
  - tool:
      call: mcp
  - nested:
      do:
        - first:
            set:
              a: 1
        - second:
            set:
              b: 2
      then: end
  - catalogFn:
      call: https://github.com/example/fn@v1
  - paint:
      set:
        call: not-a-call
schedule:
  cron: '0 0 * * *'
"#;
    let definition = Definition::from_yaml(yaml).unwrap();
    assert_eq!(definition.dsl(), "1.0.3");
    assert_eq!(definition.identity().namespace(), "orders");
    assert_eq!(definition.identity().name(), "checkout");
    assert_eq!(definition.identity().version(), "1.2.3");
    assert_eq!(definition.root().name(), Some("checkout"));
    assert_eq!(definition.root().position(), "");
    assert!(matches!(definition.root().kind(), NodeKind::Root));
    flags(definition.root(), true, false, false);
    assert!(definition.root().body().get("schedule").is_some());
    assert!(
        definition
            .root()
            .children()
            .iter()
            .all(|child| child.name() != Some("schedule"))
    );

    let positions: Vec<&str> = definition.nodes().map(Node::position).collect();
    let unique: HashSet<&str> = positions.iter().copied().collect();
    assert_eq!(unique.len(), positions.len());

    let validate = definition.get("/do/0/validateOrder").unwrap();
    flags(validate, false, false, true);
    assert!(matches!(validate.kind(), NodeKind::Call(CallKind::Http)));
    assert_eq!(validate.body()["with"]["method"], "post");

    let normalize = definition.get("/do/1/normalizeOrder").unwrap();
    flags(normalize, true, false, false);
    assert!(
        matches!(normalize.kind(), NodeKind::Call(CallKind::Function(name)) if name == "normalize")
    );
    let inlined = definition.get("/do/1/normalizeOrder/_fn").unwrap();
    assert_eq!(inlined.name(), Some("normalize"));
    flags(inlined, false, false, false);
    assert!(matches!(inlined.kind(), NodeKind::Set));

    let route = definition.get("/do/2/route").unwrap();
    flags(route, true, false, false);
    assert_eq!(route.then(), &FlowDirective::Task("rush".to_string()));
    let high = definition.get("/do/2/route/switch/0/high").unwrap();
    flags(high, false, false, false);
    assert!(
        matches!(high.kind(), NodeKind::SwitchCase { when: Some(when) } if when == ".priority == \"high\"")
    );
    assert_eq!(high.then(), &FlowDirective::Task("rush".to_string()));
    let default_case = definition.get("/do/2/route/switch/1/default").unwrap();
    assert_eq!(default_case.then(), &FlowDirective::Continue);

    let rush = definition.get("/do/3/rush").unwrap();
    flags(rush, true, true, false);
    assert!(matches!(rush.kind(), NodeKind::Fork { compete: true }));
    let ship = definition.get("/do/3/rush/fork/branches/0/ship").unwrap();
    flags(ship, false, false, true);
    assert!(matches!(ship.kind(), NodeKind::Run(RunKind::Shell)));
    let note = definition.get("/do/3/rush/fork/branches/1/note").unwrap();
    flags(note, false, false, true);
    assert!(matches!(note.kind(), NodeKind::Emit));

    flags(
        definition.get("/do/4/waitForPay").unwrap(),
        false,
        true,
        false,
    );
    let listen = definition.get("/do/5/listenPaid").unwrap();
    flags(listen, true, true, false);
    flags(
        definition
            .get("/do/5/listenPaid/foreach/do/0/record")
            .unwrap(),
        false,
        false,
        false,
    );

    let charge = definition.get("/do/6/charge").unwrap();
    flags(charge, true, true, false);
    assert!(matches!(charge.kind(), NodeKind::Try { retry: true }));
    flags(
        definition.get("/do/6/charge/try/0/callPay").unwrap(),
        false,
        false,
        true,
    );
    flags(
        definition.get("/do/6/charge/catch/do/0/fallback").unwrap(),
        false,
        false,
        false,
    );
    assert_eq!(charge.body()["catch"]["retry"]["delay"]["seconds"], 1);

    flags(definition.get("/do/7/fanout").unwrap(), true, false, false);
    flags(
        definition.get("/do/7/fanout/do/0/mark").unwrap(),
        false,
        false,
        false,
    );

    let child = definition.get("/do/8/child").unwrap();
    flags(child, false, true, true);
    assert!(matches!(child.kind(), NodeKind::Run(RunKind::Workflow)));
    flags(
        definition.get("/do/9/scripted").unwrap(),
        false,
        false,
        true,
    );
    assert!(matches!(
        definition.get("/do/9/scripted").unwrap().kind(),
        NodeKind::Run(RunKind::Script)
    ));
    flags(definition.get("/do/10/boxed").unwrap(), false, false, true);
    assert!(matches!(
        definition.get("/do/10/boxed").unwrap().kind(),
        NodeKind::Run(RunKind::Container)
    ));

    flags(definition.get("/do/11/open").unwrap(), false, false, true);
    flags(definition.get("/do/12/bus").unwrap(), false, false, true);
    flags(definition.get("/do/13/agent").unwrap(), false, false, true);
    flags(definition.get("/do/14/tool").unwrap(), false, false, true);

    let nested = definition.get("/do/15/nested").unwrap();
    flags(nested, true, false, false);
    assert_eq!(nested.then(), &FlowDirective::End);
    assert!(definition.get("/do/15/nested/do/0/first").is_some());
    assert!(definition.get("/do/15/nested/do/1/second").is_some());

    let catalog = definition.get("/do/16/catalogFn").unwrap();
    flags(catalog, true, false, false);
    assert!(catalog.children().is_empty());

    let paint = definition.get("/do/17/paint").unwrap();
    flags(paint, false, false, false);
    assert!(paint.children().is_empty());
    assert_eq!(paint.body()["set"]["call"], "not-a-call");
}

#[test]
fn yaml_and_json_share_a_content_hash() {
    let yaml = r#"
document:
  version: '1.0.0'
  name: paint
  namespace: default
  dsl: '1.0.0'
do: []
"#;
    let json = r#"{
  "do": [],
  "document": {
    "dsl": "1.0.0",
    "namespace": "default",
    "name": "paint",
    "version": "1.0.0"
  }
}"#;
    let from_yaml = Definition::from_yaml(yaml).unwrap();
    let from_json = Definition::from_json(json).unwrap();
    assert_eq!(from_yaml.content_hash(), from_json.content_hash());
    assert_eq!(from_yaml, from_json);

    let commented = Definition::from_yaml(
        "document:\n  dsl: '1.0.0' # comment\n  namespace: default\n  name: paint\n  version: '1.0.0'\ndo: []\n",
    )
    .unwrap();
    assert_eq!(commented.content_hash(), from_json.content_hash());

    let changed = Definition::from_json(
        r#"{"do":[],"document":{"dsl":"1.0.0","name":"paint","namespace":"default","version":"1.0.1"}}"#,
    )
    .unwrap();
    assert_ne!(changed.content_hash(), from_json.content_hash());
    assert_eq!(from_json.content_hash().to_hex().len(), 64);

    assert_eq!(
        canonical_json(from_yaml.root().body()),
        canonical_json(from_json.root().body())
    );
}

#[test]
fn try_without_retry_is_not_blocking_and_named_retry_is() {
    let plain = workflow(
        r#"
  - attempt:
      try:
        - step:
            set:
              n: 1
      catch:
        do:
          - recover:
              set:
                n: 0
"#,
    );
    let attempt = plain.get("/do/0/attempt").unwrap();
    flags(attempt, true, false, false);
    assert!(matches!(attempt.kind(), NodeKind::Try { retry: false }));
    assert_eq!(attempt.then(), &FlowDirective::Continue);

    let named = workflow(
        r#"
  - attempt:
      try:
        - step:
            set:
              n: 1
      catch:
        retry: defaultPolicy
"#,
    );
    flags(named.get("/do/0/attempt").unwrap(), true, true, false);

    let exit = workflow(
        r#"
  - leave:
      set:
        done: true
      then: exit
"#,
    );
    assert_eq!(
        exit.get("/do/0/leave").unwrap().then(),
        &FlowDirective::Exit
    );
}

#[test]
fn escapes_json_pointer_tokens() {
    let definition = workflow(
        r#"
  - "a/b~c":
      set:
        ok: true
"#,
    );
    let node = definition.get("/do/0/a~1b~0c").unwrap();
    assert_eq!(node.name(), Some("a/b~c"));
}

#[test]
fn rejects_malformed_documents() {
    let error = Definition::from_yaml("document: [").unwrap_err();
    assert!(matches!(error, BuildError::Parse { format: "yaml", .. }));

    let error = Definition::from_json("[]").unwrap_err();
    assert!(matches!(error, BuildError::ExpectedObject { .. }));

    let error = workflow_error(
        r#"
document:
  dsl: '1.0.0'
  name: paint
  version: '1.0.0'
do: []
"#,
    );
    assert!(matches!(
        error,
        BuildError::Missing {
            field: "namespace",
            ..
        }
    ));

    let error = workflow_error(
        r#"
  - both:
      call: http
      set:
        n: 1
"#,
    );
    assert!(matches!(error, BuildError::TaskType { .. }));

    let error = workflow_error(
        r#"
  - pair:
      set:
        n: 1
      extra: true
"#,
    );
    assert!(matches!(error, BuildError::UnknownField { .. }));

    let error = workflow_error(
        r#"
  - first:
      set:
        n: 1
    second:
      set:
        n: 2
"#,
    );
    assert!(matches!(error, BuildError::TaskItem { .. }), "{error}");
}

#[test]
fn rejects_a_function_cycle() {
    let yaml = r#"
document:
  dsl: '1.0.0'
  namespace: default
  name: cycle
  version: '1.0.0'
use:
  functions:
    a:
      call: b
    b:
      call: a
do:
  - start:
      call: a
"#;
    let error = Definition::from_yaml(yaml).unwrap_err();
    assert!(matches!(error, BuildError::FunctionCycle { .. }), "{error}");
}

#[test]
fn definition_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Definition>();
}

fn workflow(tasks: &str) -> Definition {
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: sample\n  version: '1.0.0'\ndo:\n{tasks}"
    );
    Definition::from_yaml(&yaml).unwrap_or_else(|error| panic!("{error}"))
}

fn workflow_error(body: &str) -> BuildError {
    if body.contains("document:") {
        return Definition::from_yaml(body).unwrap_err();
    }
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: default\n  name: sample\n  version: '1.0.0'\ndo:\n{body}"
    );
    Definition::from_yaml(&yaml).unwrap_err()
}
