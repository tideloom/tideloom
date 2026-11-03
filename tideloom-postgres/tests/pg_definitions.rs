//! PgDefinitions against a real PostgreSQL.
//!
//! Each test gets its own database. The URL comes from
//! `TIDELOOM_TEST_DATABASE_URL`; a missing URL skips, which is how CI on
//! macOS and Windows stays green without Postgres.

use std::sync::Arc;

use tideloom_core::Definition;
use tideloom_core::Timestamp;
use tideloom_runtime::DefinitionKey;
use tideloom_runtime::DefinitionSource;
use tideloom_runtime::StoreError;
use uuid::Uuid;

fn at(millis: u64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn skip_without_database() -> bool {
    if std::env::var_os("TIDELOOM_TEST_DATABASE_URL").is_none() {
        eprintln!("skipping PgDefinitions tests: TIDELOOM_TEST_DATABASE_URL is unset");
        return true;
    }
    false
}

fn document(namespace: &str, name: &str, version: &str, color: &str) -> Definition {
    let yaml = format!(
        "document:\n  dsl: '1.0.0'\n  namespace: {namespace}\n  name: {name}\n  version: '{version}'\ndo:\n  - paint:\n      set:\n        color: {color}\n"
    );
    Definition::from_yaml(&yaml).unwrap_or_else(|error| panic!("{error}"))
}

struct Schema {
    url: String,
    name: String,
}

impl Schema {
    async fn create() -> Self {
        let url = std::env::var("TIDELOOM_TEST_DATABASE_URL").unwrap_or_else(|_| {
            panic!("TIDELOOM_TEST_DATABASE_URL is required once a test has started")
        });
        let name = format!("s4_{}", Uuid::now_v7().simple());
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("admin connection");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("create database");
        admin.close().await;
        let test_url = replace_database(&url, &name);
        let definitions = tideloom_postgres::PgDefinitions::connect(&test_url)
            .await
            .expect("connect");
        definitions.migrate().await.expect("migrate");
        Self {
            url: test_url,
            name,
        }
    }

    async fn definitions(&self) -> tideloom_postgres::PgDefinitions {
        tideloom_postgres::PgDefinitions::connect(&self.url)
            .await
            .expect("connect")
    }
}

impl Drop for Schema {
    fn drop(&mut self) {
        let url = std::env::var("TIDELOOM_TEST_DATABASE_URL").expect("url");
        let name = self.name.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async {
                let admin = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&url)
                    .await
                    .expect("admin connection");
                let _ = sqlx::query(&format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{name}'"
                ))
                .execute(&admin)
                .await;
                let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name}"))
                    .execute(&admin)
                    .await;
                admin.close().await;
            });
        })
        .join()
        .expect("drop database");
    }
}

fn replace_database(url: &str, name: &str) -> String {
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let prefix = base.rfind('/').expect("database segment");
    let mut replaced = format!("{}{name}", &base[..=prefix]);
    if !query.is_empty() {
        replaced.push('?');
        replaced.push_str(query);
    }
    replaced
}

#[tokio::test]
async fn get_pins_the_hash_and_latest_returns_the_newest_created_at() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let definitions = schema.definitions().await;
    let red = document("default", "paint", "0.1.0", "red");
    let blue = document("default", "paint", "0.1.0", "blue");
    assert_ne!(red.content_hash(), blue.content_hash());

    // The later document is published first, with an earlier caller timestamp.
    // latest must follow created_at, not insertion order or the database clock.
    let blue_key = definitions.publish(blue.clone(), at(20)).await.unwrap();
    let red_key = definitions.publish(red.clone(), at(10)).await.unwrap();

    let pinned = definitions.get(&red_key).await.unwrap();
    assert_eq!(pinned.identity().name(), "paint");
    assert_eq!(pinned.content_hash(), red.content_hash());
    assert_eq!(
        pinned.get("/do/0/paint").unwrap().body()["set"]["color"],
        "red"
    );
    let other = definitions.get(&blue_key).await.unwrap();
    assert_eq!(
        other.get("/do/0/paint").unwrap().body()["set"]["color"],
        "blue"
    );

    let (latest_key, latest) = definitions
        .latest("default", "paint", "0.1.0")
        .await
        .unwrap();
    assert_eq!(latest_key, blue_key);
    assert_eq!(latest.content_hash(), blue.content_hash());

    let missing = DefinitionKey {
        namespace: "default".into(),
        name: "paint".into(),
        version: "0.1.0".into(),
        content_hash: "0".repeat(64),
    };
    let error = definitions.get(&missing).await.unwrap_err();
    assert!(
        matches!(&error, StoreError::Backend(detail) if detail.contains("is not published")),
        "{error}"
    );
    let error = definitions
        .latest("default", "missing", "0.1.0")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::Backend(detail) if detail.contains("is not published")),
        "{error}"
    );
}

#[tokio::test]
async fn republishing_the_same_hash_is_a_no_op_and_a_different_hash_is_a_new_row() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let definitions = schema.definitions().await;
    let red = document("default", "paint", "0.1.0", "red");
    let key = definitions.publish(red.clone(), at(10)).await.unwrap();
    let again = definitions.publish(red.clone(), at(99)).await.unwrap();
    assert_eq!(again, key);

    let created: sqlx::types::time::OffsetDateTime =
        sqlx::query_scalar("SELECT created_at FROM definition WHERE content_hash = $1")
            .bind(&key.content_hash)
            .fetch_one(definitions.pool())
            .await
            .unwrap();
    assert_eq!(
        created,
        sqlx::types::time::OffsetDateTime::from_unix_timestamp_nanos(10 * 1_000_000).unwrap()
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM definition")
        .fetch_one(definitions.pool())
        .await
        .unwrap();
    assert_eq!(rows, 1);

    let blue = document("default", "paint", "0.1.0", "blue");
    let blue_key = definitions.publish(blue, at(11)).await.unwrap();
    assert_ne!(blue_key.content_hash, key.content_hash);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM definition")
        .fetch_one(definitions.pool())
        .await
        .unwrap();
    assert_eq!(rows, 2);
    // The original row's timestamp did not move when the new hash landed.
    let still: sqlx::types::time::OffsetDateTime =
        sqlx::query_scalar("SELECT created_at FROM definition WHERE content_hash = $1")
            .bind(&key.content_hash)
            .fetch_one(definitions.pool())
            .await
            .unwrap();
    assert_eq!(still, created);
}

#[tokio::test]
async fn yaml_and_json_of_the_same_document_share_one_row() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let definitions = schema.definitions().await;
    let yaml = document("shop", "order", "1.2.3", "green");
    let json = Definition::from_json(
        r#"{"document":{"dsl":"1.0.0","namespace":"shop","name":"order","version":"1.2.3"},"do":[{"paint":{"set":{"color":"green"}}}]}"#,
    )
    .unwrap();
    assert_eq!(yaml.content_hash(), json.content_hash());
    let from_yaml = definitions.publish(yaml, at(1)).await.unwrap();
    let from_json = definitions.publish(json, at(2)).await.unwrap();
    assert_eq!(from_yaml, from_json);
    let loaded = definitions.get(&from_json).await.unwrap();
    assert_eq!(loaded.identity().namespace(), "shop");
    assert_eq!(loaded.identity().version(), "1.2.3");
    assert_eq!(loaded.content_hash().to_hex(), from_yaml.content_hash);
}

#[tokio::test]
async fn a_definition_source_object_resolves_a_published_child_target() {
    if skip_without_database() {
        return;
    }
    let schema = Schema::create().await;
    let definitions = Arc::new(schema.definitions().await);
    let child = document("default", "child", "0.1.0", "red");
    definitions.publish(child, at(5)).await.unwrap();
    let source: Arc<dyn DefinitionSource> = definitions;
    let (key, loaded) = source.latest("default", "child", "0.1.0").await.unwrap();
    assert_eq!(loaded.identity().name(), "child");
    assert_eq!(key.content_hash.len(), 64);
    let pinned = source.get(&key).await.unwrap();
    assert_eq!(pinned.content_hash(), loaded.content_hash());
}
