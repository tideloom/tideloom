//! PostgreSQL [`DefinitionSource`](tideloom_runtime::DefinitionSource).
//!
//! A definition is immutable. Publishing the same content hash again is a
//! no-op; a different hash under the same identity is a new row. Neither
//! statement reads `now()`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Json;
use sqlx::types::time::OffsetDateTime;
use tideloom_core::Definition;
use tideloom_core::Timestamp;
use tideloom_runtime::DefinitionKey;
use tideloom_runtime::DefinitionSource;
use tideloom_runtime::StoreError;

const LATEST_COLUMNS: &str = "namespace, name, version, content_hash, document_json";

/// Definitions in PostgreSQL, keyed by identity and content hash.
///
/// `get` pins a run to the exact tree it started with. `latest` matches the
/// identity triple and, when several hashes share it, returns the row with
/// the newest caller-supplied `created_at`.
#[derive(Clone, Debug)]
pub struct PgDefinitions {
    pool: PgPool,
}

impl PgDefinitions {
    /// Open a pool. Does not migrate; call [`PgDefinitions::migrate`] once per database.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] when the URL is rejected or the pool
    /// cannot connect.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(url)
            .await
            .map_err(backend)?;
        Ok(Self { pool })
    }

    /// Apply embedded migrations. Idempotent: an already-migrated database is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] when a migration statement fails.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    /// The pool, for tests that count rows on the same database.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Register a definition and return the key an instance would be created with.
    ///
    /// `now` is written as both `created_at` and `updated_at` on a new row.
    /// The same hash under the same identity leaves the existing row alone,
    /// including its timestamps. A different hash is a new row.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Backend`] when the insert fails or `now` cannot
    /// be stored as `TIMESTAMPTZ`.
    pub async fn publish(
        &self,
        definition: Definition,
        now: Timestamp,
    ) -> Result<DefinitionKey, StoreError> {
        let key = key_of(&definition);
        let document = document_of(&definition)?;
        let now = timestamp(now)?;
        sqlx::query(
            "INSERT INTO definition \
             (namespace, name, version, content_hash, document_json, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (namespace, name, version, content_hash) DO NOTHING",
        )
        .bind(&key.namespace)
        .bind(&key.name)
        .bind(&key.version)
        .bind(&key.content_hash)
        .bind(document)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(key)
    }
}

#[async_trait]
impl DefinitionSource for PgDefinitions {
    async fn get(&self, key: &DefinitionKey) -> Result<Arc<Definition>, StoreError> {
        let document = sqlx::query_scalar::<_, Json<Value>>(
            "SELECT document_json FROM definition \
             WHERE namespace = $1 AND name = $2 AND version = $3 AND content_hash = $4",
        )
        .bind(&key.namespace)
        .bind(&key.name)
        .bind(&key.version)
        .bind(&key.content_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?
        .ok_or_else(|| unpublished(key))?;
        rebuild(key, document.0)
    }

    async fn latest(
        &self,
        namespace: &str,
        name: &str,
        version: &str,
    ) -> Result<(DefinitionKey, Arc<Definition>), StoreError> {
        let row = sqlx::query_as::<_, DefinitionRow>(&format!(
            "SELECT {LATEST_COLUMNS} FROM definition \
             WHERE namespace = $1 AND name = $2 AND version = $3 \
             ORDER BY created_at DESC \
             LIMIT 1"
        ))
        .bind(namespace)
        .bind(name)
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?
        .ok_or_else(|| {
            StoreError::Backend(format!(
                "definition {namespace}/{name}@{version} is not published"
            ))
        })?;
        let key = DefinitionKey {
            namespace: row.namespace,
            name: row.name,
            version: row.version,
            content_hash: row.content_hash,
        };
        let definition = rebuild(&key, row.document_json.0)?;
        Ok((key, definition))
    }
}

#[derive(Debug, sqlx::FromRow)]
struct DefinitionRow {
    namespace: String,
    name: String,
    version: String,
    content_hash: String,
    document_json: Json<Value>,
}

fn document_of(definition: &Definition) -> Result<Json<Value>, StoreError> {
    let body = definition.root().body();
    let Value::Object(_) = body else {
        return Err(StoreError::Backend(
            "definition root body is not an object".into(),
        ));
    };
    Ok(Json(body.clone()))
}

fn rebuild(key: &DefinitionKey, document: Value) -> Result<Arc<Definition>, StoreError> {
    let definition = Definition::from_value(document).map_err(|error| {
        StoreError::Backend(format!(
            "stored definition {}/{}@{} ({}) does not build: {error}",
            key.namespace, key.name, key.version, key.content_hash
        ))
    })?;
    let stored = key_of(&definition);
    if stored != *key {
        return Err(StoreError::Backend(format!(
            "stored definition {}/{}@{} ({}) rebuilt as {}/{}@{} ({})",
            key.namespace,
            key.name,
            key.version,
            key.content_hash,
            stored.namespace,
            stored.name,
            stored.version,
            stored.content_hash
        )));
    }
    Ok(Arc::new(definition))
}

fn key_of(definition: &Definition) -> DefinitionKey {
    let identity = definition.identity();
    DefinitionKey {
        namespace: identity.namespace().to_string(),
        name: identity.name().to_string(),
        version: identity.version().to_string(),
        content_hash: definition.content_hash().to_hex(),
    }
}

fn unpublished(key: &DefinitionKey) -> StoreError {
    StoreError::Backend(format!(
        "definition {}/{}@{} ({}) is not published",
        key.namespace, key.name, key.version, key.content_hash
    ))
}

fn timestamp(value: Timestamp) -> Result<OffsetDateTime, StoreError> {
    let millis = i128::from(value.millis());
    OffsetDateTime::from_unix_timestamp_nanos(millis * 1_000_000).map_err(|error| {
        StoreError::Backend(format!(
            "timestamp {} is out of range: {error}",
            value.millis()
        ))
    })
}

fn backend(error: impl ToString) -> StoreError {
    StoreError::Backend(error.to_string())
}
