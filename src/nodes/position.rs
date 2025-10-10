use serde::{Deserialize, Serialize};

/// JSON-pointer-like position for addressing nodes in a workflow graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodePosition {
    path: Vec<String>,
}

impl NodePosition {
    pub fn root() -> Self { Self { path: vec![] } }

    pub fn add_name(mut self, name: impl Into<String>) -> Self {
        let n = name.into();
        assert!(!n.contains('/'), "name must not contain '/' ");
        self.path.push(n);
        self
    }

    pub fn add_token(mut self, token: impl Into<String>) -> Self {
        self.path.push(token.into());
        self
    }

    pub fn add_index(mut self, index: usize) -> Self {
        self.path.push(index.to_string());
        self
    }

    pub fn parent(&self) -> Option<Self> {
        if self.path.is_empty() { None } else { Some(Self { path: self.path[..self.path.len()-1].to_vec() }) }
    }

    pub fn as_pointer(&self) -> String {
        match self.path.is_empty() {
            true => String::new(),
            false => format!("/{}", self.path.join("/")),
        }
    }
}

