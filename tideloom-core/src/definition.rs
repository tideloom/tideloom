use serde_json::Map;
use serde_json::Value;

use crate::error::BuildError;
use crate::hash::ContentHash;
use crate::node::CallKind;
use crate::node::FlowDirective;
use crate::node::Identity;
use crate::node::Node;
use crate::node::NodeKind;
use crate::node::RunKind;

const ROOT_KEYS: &[&str] = &[
    "do", "document", "evaluate", "input", "output", "schedule", "timeout", "use",
];
const USE_KEYS: &[&str] = &[
    "authentications",
    "catalogs",
    "errors",
    "extensions",
    "functions",
    "retries",
    "secrets",
    "timeouts",
];
const DOCUMENT_KEYS: &[&str] = &[
    "dsl",
    "metadata",
    "name",
    "namespace",
    "summary",
    "tags",
    "title",
    "version",
];

/// Immutable workflow definition built from a Serverless Workflow document.
///
/// Construct it with [`Definition::from_yaml`] or [`Definition::from_json`].
/// The value does not change afterward: every field is private, and no method
/// takes `&mut self`. Share one definition across runs with `Arc<Definition>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Definition {
    identity: Identity,
    dsl: String,
    content_hash: ContentHash,
    root: Node,
}

impl Definition {
    /// Parse a workflow YAML document and build the immutable tree.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when the document is not a workflow this build
    /// understands.
    ///
    /// # Examples
    ///
    /// ```
    /// use tideloom_core::Definition;
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
    /// assert_eq!(definition.identity().name(), "paint");
    /// let node = definition.get("/do/0/setColor").unwrap();
    /// assert!(!node.composite());
    /// assert!(!node.is_block_boundary());
    /// ```
    pub fn from_yaml(yaml: &str) -> Result<Self, BuildError> {
        let value: serde_yaml::Value =
            serde_yaml::from_str(yaml).map_err(|error| BuildError::Parse {
                format: "yaml",
                message: error.to_string(),
            })?;
        let document = serde_json::to_value(value).map_err(|error| BuildError::Parse {
            format: "yaml",
            message: error.to_string(),
        })?;
        Self::from_value(document)
    }

    /// Parse a workflow JSON document and build the immutable tree.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when the document is not a workflow this build
    /// understands.
    pub fn from_json(json: &str) -> Result<Self, BuildError> {
        let document = serde_json::from_str(json).map_err(|error| BuildError::Parse {
            format: "json",
            message: error.to_string(),
        })?;
        Self::from_value(document)
    }

    /// Build from a parsed JSON document.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] when the document is not a workflow this build
    /// understands.
    pub fn from_value(document: Value) -> Result<Self, BuildError> {
        let content_hash = ContentHash::of(&document);
        let root_object = document
            .as_object()
            .ok_or_else(|| BuildError::expected_object("/"))?;
        reject_unknown(root_object, "/", ROOT_KEYS)?;
        let document_object = required_object(root_object, "", "document")?;
        reject_unknown(document_object, "/document", DOCUMENT_KEYS)?;
        let dsl = required_string(document_object, "/document", "dsl")?;
        let namespace = required_string(document_object, "/document", "namespace")?;
        let name = required_string(document_object, "/document", "name")?;
        let version = required_string(document_object, "/document", "version")?;
        let functions = load_functions(root_object)?;
        let mut builder = Builder {
            functions,
            stack: Vec::new(),
        };
        let do_value = root_object
            .get("do")
            .ok_or_else(|| BuildError::missing("", "do"))?;
        let children = builder.task_list(do_value, "/do")?;
        let root = Node::new(
            String::new(),
            Some(name.clone()),
            NodeKind::Root,
            FlowDirective::Continue,
            children,
            document,
        );
        Ok(Self {
            identity: Identity::new(namespace, name, version),
            dsl,
            content_hash,
            root,
        })
    }

    /// `(namespace, name, version)` of the document.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// `document.dsl`.
    pub fn dsl(&self) -> &str {
        &self.dsl
    }

    /// Content hash of the canonical document.
    pub fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    /// Synthetic root. Its children are the top-level `do` tasks.
    pub fn root(&self) -> &Node {
        &self.root
    }

    /// Node at a JSON Pointer, including the empty pointer for the root.
    pub fn get(&self, position: &str) -> Option<&Node> {
        self.root.walk().find(|node| node.position() == position)
    }

    /// Root, then every descendant.
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.root.walk()
    }
}

struct Builder<'a> {
    functions: Vec<(&'a str, &'a Value)>,
    stack: Vec<String>,
}

impl<'a> Builder<'a> {
    fn function(&self, name: &str) -> Option<&'a Value> {
        self.functions
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| *value)
    }

    fn task_list(&mut self, value: &Value, list_pointer: &str) -> Result<Vec<Node>, BuildError> {
        let items = value
            .as_array()
            .ok_or_else(|| BuildError::expected_array(list_pointer))?;
        let mut nodes = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let item_pointer = pointer(list_pointer, &[&index.to_string()]);
            let object = item.as_object().ok_or_else(|| BuildError::TaskItem {
                position: item_pointer.clone(),
            })?;
            if object.len() != 1 {
                return Err(BuildError::TaskItem {
                    position: item_pointer,
                });
            }
            let (name, body) = object.iter().next().expect("len checked");
            let position = pointer(list_pointer, &[&index.to_string(), name]);
            nodes.push(self.task(name, body, &position)?);
        }
        Ok(nodes)
    }

    fn task(&mut self, name: &str, body: &Value, position: &str) -> Result<Node, BuildError> {
        let object = body
            .as_object()
            .ok_or_else(|| BuildError::expected_object(position))?;
        let kind = self.kind(object, name, position)?;
        reject_unknown(object, position, allowed_keys(&kind))?;
        let then = flow_directive(object, position, "then")?;
        let children = self.children(&kind, object, position)?;
        Ok(Node::new(
            position.to_string(),
            Some(name.to_string()),
            kind,
            then,
            children,
            body.clone(),
        ))
    }

    fn kind(
        &self,
        object: &Map<String, Value>,
        name: &str,
        position: &str,
    ) -> Result<NodeKind, BuildError> {
        if object.contains_key("for") {
            reject_other_discriminants(object, name, position, &["do", "for"])?;
            return Ok(NodeKind::For);
        }
        if object.contains_key("try") {
            reject_other_discriminants(object, name, position, &["try"])?;
            let retry = retry_flag(object, position)?;
            return Ok(NodeKind::Try { retry });
        }
        let found = DISCRIMINANTS
            .iter()
            .filter(|key| object.contains_key(**key))
            .copied()
            .collect::<Vec<_>>();
        if found.len() != 1 {
            return Err(BuildError::TaskType {
                position: position.to_string(),
                name: name.to_string(),
            });
        }
        match found[0] {
            "call" => {
                let call = required_string(object, position, "call")?;
                Ok(NodeKind::Call(CallKind::from_call(&call)))
            }
            "do" => Ok(NodeKind::Do),
            "emit" => Ok(NodeKind::Emit),
            "fork" => {
                let compete = fork_compete(object, position)?;
                Ok(NodeKind::Fork { compete })
            }
            "listen" => Ok(NodeKind::Listen),
            "raise" => Ok(NodeKind::Raise),
            "run" => Ok(NodeKind::Run(run_kind(object, position)?)),
            "set" => Ok(NodeKind::Set),
            "switch" => Ok(NodeKind::Switch),
            "wait" => Ok(NodeKind::Wait),
            other => Err(BuildError::TaskType {
                position: position.to_string(),
                name: other.to_string(),
            }),
        }
    }

    fn children(
        &mut self,
        kind: &NodeKind,
        object: &Map<String, Value>,
        position: &str,
    ) -> Result<Vec<Node>, BuildError> {
        match kind {
            NodeKind::Do => self.task_list(
                required(object, position, "do")?,
                &pointer(position, &["do"]),
            ),
            NodeKind::For => {
                let config = required_object(object, position, "for")?;
                let config_position = pointer(position, &["for"]);
                reject_unknown(config, &config_position, &["at", "each", "in"])?;
                if !config.contains_key("in") {
                    return Err(BuildError::missing(&config_position, "in"));
                }
                self.task_list(
                    required(object, position, "do")?,
                    &pointer(position, &["do"]),
                )
            }
            NodeKind::Fork { .. } => {
                let fork = required_object(object, position, "fork")?;
                let branches = fork
                    .get("branches")
                    .ok_or_else(|| BuildError::missing(pointer(position, &["fork"]), "branches"))?;
                self.task_list(branches, &pointer(position, &["fork", "branches"]))
            }
            NodeKind::Try { .. } => {
                let mut children = self.task_list(
                    required(object, position, "try")?,
                    &pointer(position, &["try"]),
                )?;
                let catch = required_object(object, position, "catch")?;
                reject_unknown(catch, &pointer(position, &["catch"]), CATCH_KEYS)?;
                if let Some(catch_do) = catch.get("do") {
                    children
                        .extend(self.task_list(catch_do, &pointer(position, &["catch", "do"]))?);
                }
                Ok(children)
            }
            NodeKind::Switch => self.switch_cases(required(object, position, "switch")?, position),
            NodeKind::Listen => self.listen_foreach(object, position),
            NodeKind::Call(CallKind::Function(function_name)) => {
                self.inline_function(function_name, position)
            }
            NodeKind::Root
            | NodeKind::Set
            | NodeKind::SwitchCase { .. }
            | NodeKind::Raise
            | NodeKind::Wait
            | NodeKind::Emit
            | NodeKind::Call(_)
            | NodeKind::Run(_) => Ok(Vec::new()),
        }
    }

    fn switch_cases(&self, value: &Value, position: &str) -> Result<Vec<Node>, BuildError> {
        let list_pointer = pointer(position, &["switch"]);
        let items = value
            .as_array()
            .ok_or_else(|| BuildError::expected_array(&list_pointer))?;
        if items.is_empty() {
            return Err(BuildError::EmptySwitch {
                position: position.to_string(),
            });
        }
        let mut nodes = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let item_pointer = pointer(&list_pointer, &[&index.to_string()]);
            let object = item.as_object().ok_or_else(|| BuildError::TaskItem {
                position: item_pointer.clone(),
            })?;
            if object.len() != 1 {
                return Err(BuildError::TaskItem {
                    position: item_pointer,
                });
            }
            let (case_name, body) = object.iter().next().expect("len checked");
            let case_position = pointer(&list_pointer, &[&index.to_string(), case_name]);
            let case = body
                .as_object()
                .ok_or_else(|| BuildError::expected_object(&case_position))?;
            reject_unknown(case, &case_position, &["then", "when"])?;
            let when = match case.get("when") {
                None => None,
                Some(Value::String(text)) => Some(text.clone()),
                Some(_) => {
                    return Err(BuildError::expected_string(pointer(
                        &case_position,
                        &["when"],
                    )));
                }
            };
            let then = match case.get("then") {
                Some(Value::String(text)) => FlowDirective::parse(text),
                Some(_) => {
                    return Err(BuildError::expected_string(pointer(
                        &case_position,
                        &["then"],
                    )));
                }
                None => {
                    return Err(BuildError::SwitchThen {
                        position: case_position,
                    });
                }
            };
            nodes.push(Node::new(
                case_position,
                Some(case_name.clone()),
                NodeKind::SwitchCase { when },
                then,
                Vec::new(),
                body.clone(),
            ));
        }
        Ok(nodes)
    }

    fn listen_foreach(
        &mut self,
        object: &Map<String, Value>,
        position: &str,
    ) -> Result<Vec<Node>, BuildError> {
        let listen = required_object(object, position, "listen")?;
        let listen_position = pointer(position, &["listen"]);
        match listen.get("to") {
            Some(value) if value.is_object() => {}
            Some(_) => {
                return Err(BuildError::expected_object(pointer(
                    &listen_position,
                    &["to"],
                )));
            }
            None => return Err(BuildError::missing(&listen_position, "to")),
        }
        reject_unknown(listen, &listen_position, &["read", "to"])?;
        let Some(foreach) = object.get("foreach") else {
            return Ok(Vec::new());
        };
        let foreach_object = foreach
            .as_object()
            .ok_or_else(|| BuildError::expected_object(pointer(position, &["foreach"])))?;
        reject_unknown(
            foreach_object,
            &pointer(position, &["foreach"]),
            &["at", "do", "export", "item", "output"],
        )?;
        match foreach_object.get("do") {
            Some(tasks) => self.task_list(tasks, &pointer(position, &["foreach", "do"])),
            None => Ok(Vec::new()),
        }
    }

    fn inline_function(&mut self, name: &str, position: &str) -> Result<Vec<Node>, BuildError> {
        let Some(body) = self.function(name).cloned() else {
            return Ok(Vec::new());
        };
        if self.stack.iter().any(|stacked| stacked == name) {
            return Err(BuildError::FunctionCycle {
                position: position.to_string(),
                name: name.to_string(),
            });
        }
        self.stack.push(name.to_string());
        let child = self.task(name, &body, &pointer(position, &["_fn"]));
        self.stack.pop();
        Ok(vec![child?])
    }
}

const DISCRIMINANTS: &[&str] = &[
    "call", "do", "emit", "fork", "listen", "raise", "run", "set", "switch", "wait",
];

const CATCH_KEYS: &[&str] = &["as", "do", "errors", "exceptWhen", "retry", "then", "when"];

fn allowed_keys(kind: &NodeKind) -> &'static [&'static str] {
    match kind {
        NodeKind::Call(_) => &[
            "call", "export", "if", "input", "metadata", "output", "then", "timeout", "with",
        ],
        NodeKind::Do => &[
            "do", "export", "if", "input", "metadata", "output", "then", "timeout",
        ],
        NodeKind::Emit => &[
            "emit", "export", "if", "input", "metadata", "output", "then", "timeout",
        ],
        NodeKind::For => &[
            "do", "export", "for", "if", "input", "metadata", "output", "then", "timeout", "while",
        ],
        NodeKind::Fork { .. } => &[
            "export", "fork", "if", "input", "metadata", "output", "then", "timeout",
        ],
        NodeKind::Listen => &[
            "export", "foreach", "if", "input", "listen", "metadata", "output", "then", "timeout",
        ],
        NodeKind::Raise => &[
            "export", "if", "input", "metadata", "output", "raise", "then", "timeout",
        ],
        NodeKind::Run(_) => &[
            "export", "if", "input", "metadata", "output", "run", "then", "timeout",
        ],
        NodeKind::Set => &[
            "export", "if", "input", "metadata", "output", "set", "then", "timeout",
        ],
        NodeKind::Switch => &[
            "export", "if", "input", "metadata", "output", "switch", "then", "timeout",
        ],
        NodeKind::Try { .. } => &[
            "catch", "export", "if", "input", "metadata", "output", "then", "timeout", "try",
        ],
        NodeKind::Wait => &[
            "export", "if", "input", "metadata", "output", "then", "timeout", "wait",
        ],
        NodeKind::Root | NodeKind::SwitchCase { .. } => &[],
    }
}

fn reject_unknown(
    object: &Map<String, Value>,
    position: &str,
    allowed: &[&str],
) -> Result<(), BuildError> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(BuildError::UnknownField {
                position: position.to_string(),
                field: key.clone(),
            });
        }
    }
    Ok(())
}

fn reject_other_discriminants(
    object: &Map<String, Value>,
    name: &str,
    position: &str,
    allowed: &[&str],
) -> Result<(), BuildError> {
    let conflict = DISCRIMINANTS
        .iter()
        .copied()
        .chain(["for", "try"])
        .any(|key| object.contains_key(key) && !allowed.contains(&key));
    if conflict {
        return Err(BuildError::TaskType {
            position: position.to_string(),
            name: name.to_string(),
        });
    }
    Ok(())
}

fn retry_flag(object: &Map<String, Value>, position: &str) -> Result<bool, BuildError> {
    let catch_pointer = pointer(position, &["catch"]);
    let catch = required_object(object, position, "catch")?;
    match catch.get("retry") {
        None => Ok(false),
        Some(Value::Object(_) | Value::String(_)) => Ok(true),
        Some(_) => Err(BuildError::RetryType {
            position: pointer(&catch_pointer, &["retry"]),
        }),
    }
}

fn fork_compete(object: &Map<String, Value>, position: &str) -> Result<bool, BuildError> {
    let fork = required_object(object, position, "fork")?;
    reject_unknown(
        fork,
        &pointer(position, &["fork"]),
        &["branches", "compete"],
    )?;
    match fork.get("compete") {
        None => Ok(false),
        Some(Value::Bool(compete)) => Ok(*compete),
        Some(_) => Err(BuildError::CompeteType {
            position: pointer(position, &["fork", "compete"]),
        }),
    }
}

fn run_kind(object: &Map<String, Value>, position: &str) -> Result<RunKind, BuildError> {
    let run = required_object(object, position, "run")?;
    reject_unknown(
        run,
        &pointer(position, &["run"]),
        &[
            "await",
            "container",
            "return",
            "script",
            "shell",
            "workflow",
        ],
    )?;
    let targets = ["container", "script", "shell", "workflow"]
        .into_iter()
        .filter(|key| run.contains_key(*key))
        .collect::<Vec<_>>();
    match targets.as_slice() {
        ["container"] => Ok(RunKind::Container),
        ["script"] => Ok(RunKind::Script),
        ["shell"] => Ok(RunKind::Shell),
        ["workflow"] => Ok(RunKind::Workflow),
        _ => Err(BuildError::RunTarget {
            position: position.to_string(),
        }),
    }
}

fn flow_directive(
    object: &Map<String, Value>,
    position: &str,
    field: &str,
) -> Result<FlowDirective, BuildError> {
    match object.get(field) {
        None => Ok(FlowDirective::Continue),
        Some(Value::String(text)) => Ok(FlowDirective::parse(text)),
        Some(_) => Err(BuildError::expected_string(pointer(position, &[field]))),
    }
}

fn load_functions(root: &Map<String, Value>) -> Result<Vec<(&str, &Value)>, BuildError> {
    let Some(use_value) = root.get("use") else {
        return Ok(Vec::new());
    };
    let use_object = use_value
        .as_object()
        .ok_or_else(|| BuildError::expected_object("/use"))?;
    reject_unknown(use_object, "/use", USE_KEYS)?;
    let Some(functions) = use_object.get("functions") else {
        return Ok(Vec::new());
    };
    let functions = functions
        .as_object()
        .ok_or_else(|| BuildError::expected_object("/use/functions"))?;
    let mut loaded = Vec::with_capacity(functions.len());
    for (name, body) in functions {
        if !body.is_object() {
            return Err(BuildError::expected_object(pointer(
                "/use/functions",
                &[name],
            )));
        }
        loaded.push((name.as_str(), body));
    }
    Ok(loaded)
}

fn required<'a>(
    object: &'a Map<String, Value>,
    position: &str,
    field: &'static str,
) -> Result<&'a Value, BuildError> {
    object
        .get(field)
        .ok_or_else(|| BuildError::missing(position, field))
}

fn required_object<'a>(
    object: &'a Map<String, Value>,
    position: &str,
    field: &'static str,
) -> Result<&'a Map<String, Value>, BuildError> {
    let value = required(object, position, field)?;
    value
        .as_object()
        .ok_or_else(|| BuildError::expected_object(pointer(position, &[field])))
}

fn required_string(
    object: &Map<String, Value>,
    position: &str,
    field: &'static str,
) -> Result<String, BuildError> {
    match required(object, position, field)? {
        Value::String(text) => Ok(text.clone()),
        _ => Err(BuildError::expected_string(pointer(position, &[field]))),
    }
}

fn pointer(base: &str, segments: &[&str]) -> String {
    let mut out = String::from(base);
    for segment in segments {
        out.push('/');
        out.push_str(&escape_token(segment));
    }
    out
}

fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}
