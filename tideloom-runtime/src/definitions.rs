use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use async_trait::async_trait;
use tideloom_core::Definition;

use crate::DefinitionKey;
use crate::StoreError;

/// Where the engine reads definitions.
///
/// Definitions are immutable and content-addressed. `get` pins a run to the
/// exact tree it started with; `latest` resolves a `run: workflow` target by
/// its identity triple when the child is created.
#[async_trait]
pub trait DefinitionSource: Send + Sync {
    /// The definition an instance was created with.
    async fn get(&self, key: &DefinitionKey) -> Result<Arc<Definition>, StoreError>;

    /// The definition published under this identity triple.
    async fn latest(
        &self,
        namespace: &str,
        name: &str,
        version: &str,
    ) -> Result<(DefinitionKey, Arc<Definition>), StoreError>;
}

/// Definitions held in memory, keyed by identity and hash.
#[derive(Debug, Default)]
pub struct MemoryDefinitions {
    by_key: RwLock<HashMap<DefinitionKey, Arc<Definition>>>,
}

impl MemoryDefinitions {
    /// Empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a definition and return the key an instance would be created with.
    pub fn publish(&self, definition: Definition) -> DefinitionKey {
        let key = key_of(&definition);
        self.by_key
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key.clone(), Arc::new(definition));
        key
    }
}

/// The key an instance of `definition` is created with.
#[must_use]
pub fn key_of(definition: &Definition) -> DefinitionKey {
    let identity = definition.identity();
    DefinitionKey {
        namespace: identity.namespace().to_string(),
        name: identity.name().to_string(),
        version: identity.version().to_string(),
        content_hash: definition.content_hash().to_hex(),
    }
}

#[async_trait]
impl DefinitionSource for MemoryDefinitions {
    async fn get(&self, key: &DefinitionKey) -> Result<Arc<Definition>, StoreError> {
        self.by_key
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned()
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "definition {}/{}@{} ({}) is not published",
                    key.namespace, key.name, key.version, key.content_hash
                ))
            })
    }

    async fn latest(
        &self,
        namespace: &str,
        name: &str,
        version: &str,
    ) -> Result<(DefinitionKey, Arc<Definition>), StoreError> {
        self.by_key
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|(key, _)| {
                key.namespace == namespace && key.name == name && key.version == version
            })
            .map(|(key, definition)| (key.clone(), Arc::clone(definition)))
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "definition {namespace}/{name}@{version} is not published"
                ))
            })
    }
}
