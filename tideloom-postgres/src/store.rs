//! PostgreSQL [`InstanceStore`](tideloom_runtime::InstanceStore).
//!
//! One `apply` is one transaction. `claim` takes its rows with
//! `FOR UPDATE SKIP LOCKED`. Neither statement reads `now()`.

use std::str::FromStr;

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::Postgres;
use sqlx::Row;
use sqlx::Transaction;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Json;
use sqlx::types::Uuid;
use sqlx::types::time::OffsetDateTime;
use tideloom_core::Counter;
use tideloom_core::ExecutionKey;
use tideloom_core::Fault;
use tideloom_core::TaskKey;
use tideloom_core::Timestamp;
use tideloom_runtime::Commit;
use tideloom_runtime::DefinitionKey;
use tideloom_runtime::InFlight;
use tideloom_runtime::Instance;
use tideloom_runtime::InstanceId;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::Lease;
use tideloom_runtime::ParentRef;
use tideloom_runtime::Status;
use tideloom_runtime::StepOutcome;
use tideloom_runtime::StepResult;
use tideloom_runtime::StoreError;
use tideloom_runtime::TaskKind;
use tideloom_runtime::WaitReason;
use tideloom_runtime::WorkerId;

/// `workflow_instance.status`. Kept beside the store so SQL strings stay numeric.
const STATUS_RUNNABLE: i16 = 0;
const STATUS_RUNNING: i16 = 1;
const STATUS_WAITING: i16 = 2;
const STATUS_COMPLETED: i16 = 3;
const STATUS_FAULTED: i16 = 4;
const STATUS_CANCELLED: i16 = 5;

/// `workflow_instance.wait_reason`, meaningful only when status is waiting.
const WAIT_TIMER: i16 = 0;
const WAIT_JOIN: i16 = 1;
const WAIT_CHILD: i16 = 2;
const WAIT_EVENTS: i16 = 3;

/// `task_execution.kind`, in the order [`TaskKind`] declares its variants.
const KIND_HTTP: i16 = 0;
const KIND_GRPC: i16 = 1;
const KIND_OPENAPI: i16 = 2;
const KIND_ASYNCAPI: i16 = 3;
const KIND_A2A: i16 = 4;
const KIND_MCP: i16 = 5;
const KIND_EMIT: i16 = 6;
const KIND_SHELL: i16 = 7;
const KIND_SCRIPT: i16 = 8;
const KIND_CONTAINER: i16 = 9;

/// `task_execution.outcome`.
const OUTCOME_PENDING: i16 = 0;
const OUTCOME_OUTPUT: i16 = 1;
const OUTCOME_FAULT: i16 = 2;
const OUTCOME_UNKNOWN: i16 = 3;

const INSTANCE_COLUMNS: &str = "\
id, created_at, updated_at, definition_ns, definition_name, definition_version, \
definition_hash, parent_id, parent_branch, root_task, status, wait_reason, \
row_version, lease_worker, lease_until, in_flight_key, in_flight_attempt, \
wake_at, continuation_json, input_json, output_json, fault_json";

/// Instances and journal rows in PostgreSQL.
///
/// A pool is shared across workers in one process. Cross-process exclusion
/// comes from `claim`'s `FOR UPDATE SKIP LOCKED` and from `apply`'s
/// `WHERE row_version = $expected`, not from a process-local lock.
#[derive(Clone, Debug)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Open a pool. Does not migrate; call [`PgStore::migrate`] once per database.
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

    /// The pool, for tests that open a second connection against the same database.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl InstanceStore for PgStore {
    async fn create(&self, instance: Instance) -> Result<(), StoreError> {
        let row = InstanceRow::try_from(instance)?;
        let inserted = sqlx::query(&format!(
            "INSERT INTO workflow_instance ({INSTANCE_COLUMNS}) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22) \
             ON CONFLICT (id) DO NOTHING"
        ))
        .bind(row.id)
        .bind(row.created_at)
        .bind(row.updated_at)
        .bind(row.definition_ns)
        .bind(row.definition_name)
        .bind(row.definition_version)
        .bind(row.definition_hash)
        .bind(row.parent_id)
        .bind(row.parent_branch)
        .bind(row.root_task)
        .bind(row.status)
        .bind(row.wait_reason)
        .bind(row.row_version)
        .bind(row.lease_worker)
        .bind(row.lease_until)
        .bind(row.in_flight_key)
        .bind(row.in_flight_attempt)
        .bind(row.wake_at)
        .bind(row.continuation_json)
        .bind(row.input_json)
        .bind(row.output_json)
        .bind(row.fault_json)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        if inserted.rows_affected() == 0 {
            return Err(StoreError::AlreadyExists(InstanceId::from_string(
                row.id.to_string(),
            )));
        }
        Ok(())
    }

    async fn get(&self, id: &InstanceId) -> Result<Instance, StoreError> {
        let uuid = parse_id(id)?;
        let row = sqlx::query_as::<_, InstanceRow>(&format!(
            "SELECT {INSTANCE_COLUMNS} FROM workflow_instance WHERE id = $1"
        ))
        .bind(uuid)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?
        .ok_or_else(|| StoreError::NotFound(id.clone()))?;
        row.try_into()
    }

    async fn claim(
        &self,
        worker: &WorkerId,
        now: Timestamp,
        until: Timestamp,
        limit: usize,
    ) -> Result<Vec<Instance>, StoreError> {
        let limit = i64::try_from(limit).map_err(|_| {
            StoreError::Backend(format!("claim limit {limit} does not fit in a bigint"))
        })?;
        let now = timestamp(now)?;
        let until = timestamp(until)?;
        // Lock the candidates, then update exactly those ids. Both statements
        // see the caller's `now`. SKIP LOCKED skips a row another transaction
        // already holds.
        let returning = returning_columns();
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let ids = sqlx::query_scalar::<_, Uuid>(&format!(
            "SELECT d.id \
             FROM workflow_instance AS d \
             WHERE ((d.status = {STATUS_RUNNABLE} \
                     AND (d.wake_at IS NULL OR d.wake_at <= $1)) \
                    OR (d.status = {STATUS_WAITING} AND d.wait_reason = {WAIT_TIMER} \
                        AND d.wake_at IS NOT NULL AND d.wake_at <= $1)) \
               AND (d.lease_until IS NULL OR d.lease_until <= $1) \
             ORDER BY d.wake_at IS NULL DESC, d.wake_at, d.id \
             LIMIT $2 \
             FOR UPDATE OF d SKIP LOCKED"
        ))
        .bind(now)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(backend)?;
        let rows = if ids.is_empty() {
            Vec::new()
        } else {
            sqlx::query_as::<_, InstanceRow>(&format!(
                "UPDATE workflow_instance AS i \
                 SET status = {STATUS_RUNNING}, wait_reason = NULL, \
                     lease_worker = $2, lease_until = $3, updated_at = $4 \
                 WHERE i.id = ANY($1) \
                 RETURNING {returning}"
            ))
            .bind(&ids)
            .bind(&worker.0)
            .bind(until)
            .bind(now)
            .fetch_all(&mut *tx)
            .await
            .map_err(backend)?
        };
        tx.commit().await.map_err(backend)?;
        let mut instances: Vec<Instance> = rows
            .into_iter()
            .map(InstanceRow::try_into)
            .collect::<Result<_, _>>()?;
        instances.sort_by(|left, right| (left.wake_at, &left.id).cmp(&(right.wake_at, &right.id)));
        Ok(instances)
    }

    async fn apply(&self, commit: Commit) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let instance = parse_id(&commit.instance)?;
        let at = timestamp(commit.at)?;
        let (status, wait_reason) = encode_status(commit.status)?;
        let in_flight = encode_in_flight(commit.in_flight.as_ref())?;
        let updated = sqlx::query(
            "UPDATE workflow_instance AS i \
             SET status = $3, wait_reason = $4, row_version = i.row_version + 1, \
                 continuation_json = $5, wake_at = $6, output_json = $7, fault_json = $8, \
                 in_flight_key = $9, in_flight_attempt = $10, \
                 lease_worker = CASE WHEN $11 THEN NULL ELSE i.lease_worker END, \
                 lease_until = CASE WHEN $11 THEN NULL ELSE i.lease_until END, \
                 updated_at = $12 \
             WHERE i.id = $1 AND i.row_version = $2",
        )
        .bind(instance)
        .bind(i64::try_from(commit.expected_version).map_err(|_| {
            StoreError::Backend(format!(
                "version {} does not fit in a bigint",
                commit.expected_version
            ))
        })?)
        .bind(status)
        .bind(wait_reason)
        .bind(encode_continuation(commit.continuation.as_deref())?)
        .bind(commit.wake_at.map(timestamp).transpose()?)
        .bind(commit.output.clone().map(Json))
        .bind(commit.fault.as_ref().map(encode_fault).map(Json))
        .bind(in_flight.as_ref().map(|value| value.0.clone()))
        .bind(in_flight.as_ref().map(|value| value.1))
        .bind(commit.release_lease)
        .bind(at)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        if updated.rows_affected() == 0 {
            let found = sqlx::query_scalar::<_, i64>(
                "SELECT row_version FROM workflow_instance WHERE id = $1",
            )
            .bind(instance)
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?;
            // Nothing else in this transaction has written. Rolling back is
            // still required so a later statement cannot land on a failed CAS.
            tx.rollback().await.map_err(backend)?;
            return match found {
                Some(found) => Err(StoreError::VersionConflict {
                    instance: commit.instance,
                    expected: commit.expected_version,
                    found: u64::try_from(found).map_err(|_| {
                        StoreError::Backend(format!("stored version {found} is negative"))
                    })?,
                }),
                None => Err(StoreError::NotFound(commit.instance)),
            };
        }

        if let Err(error) = apply_children(&mut tx, &commit, at).await {
            tx.rollback().await.map_err(backend)?;
            return Err(error);
        }
        if let Err(error) = apply_cancels(&mut tx, &commit, at).await {
            tx.rollback().await.map_err(backend)?;
            return Err(error);
        }
        if let Err(error) = apply_steps(&mut tx, &commit, at).await {
            tx.rollback().await.map_err(backend)?;
            return Err(error);
        }
        tx.commit().await.map_err(backend)?;
        Ok(())
    }

    async fn sweep(&self, before: Timestamp) -> Result<Vec<InstanceId>, StoreError> {
        let before = timestamp(before)?;
        let ids = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM workflow_instance \
             WHERE status = $1 AND lease_until IS NOT NULL AND lease_until < $2 \
             ORDER BY id",
        )
        .bind(STATUS_RUNNING)
        .bind(before)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        Ok(ids
            .into_iter()
            .map(|id| InstanceId::from_string(id.to_string()))
            .collect())
    }

    async fn children(&self, parent: &InstanceId) -> Result<Vec<Instance>, StoreError> {
        let parent = parse_id(parent)?;
        // NULL branch sorts first, matching MemoryStore's `Option<usize>` order.
        let rows = sqlx::query_as::<_, InstanceRow>(&format!(
            "SELECT {INSTANCE_COLUMNS} FROM workflow_instance \
             WHERE parent_id = $1 \
             ORDER BY parent_branch NULLS FIRST, created_at, id"
        ))
        .bind(parent)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter().map(InstanceRow::try_into).collect()
    }

    async fn steps(&self, instance: &InstanceId) -> Result<Vec<StepResult>, StoreError> {
        let instance_id = parse_id(instance)?;
        let rows = sqlx::query_as::<_, StepRow>(
            "SELECT instance_id, position, execution_key, attempt, kind, outcome, \
                    input_json, result_json, started_at, finished_at \
             FROM task_execution \
             WHERE instance_id = $1 \
             ORDER BY started_at, finished_at, position, execution_key, attempt",
        )
        .bind(instance_id)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter().map(StepRow::try_into).collect()
    }

    async fn latest_step(
        &self,
        instance: &InstanceId,
        key: &TaskKey,
    ) -> Result<Option<StepResult>, StoreError> {
        let instance_id = parse_id(instance)?;
        let row = sqlx::query_as::<_, StepRow>(
            "SELECT instance_id, position, execution_key, attempt, kind, outcome, \
                    input_json, result_json, started_at, finished_at \
             FROM task_execution \
             WHERE instance_id = $1 AND position = $2 AND execution_key = $3 \
             ORDER BY attempt DESC \
             LIMIT 1",
        )
        .bind(instance_id)
        .bind(key.position())
        .bind(key.execution_key().to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(StepRow::try_into).transpose()
    }

    async fn wake(&self, ids: &[InstanceId], now: Timestamp) -> Result<usize, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut uuids = Vec::with_capacity(ids.len());
        for id in ids {
            uuids.push(parse_id(id)?);
        }
        let now = timestamp(now)?;
        // Condition, not a CAS: only an unarmed events wait flips. A second
        // call sees the wake_at it just wrote and matches nothing.
        let flipped = sqlx::query(
            "UPDATE workflow_instance \
             SET status = $1, wait_reason = NULL, wake_at = $2, \
                 row_version = row_version + 1, updated_at = $2 \
             WHERE id = ANY($3) \
               AND status = $4 AND wait_reason = $5 AND wake_at IS NULL",
        )
        .bind(STATUS_RUNNABLE)
        .bind(now)
        .bind(&uuids)
        .bind(STATUS_WAITING)
        .bind(WAIT_EVENTS)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        usize::try_from(flipped.rows_affected())
            .map_err(|_| StoreError::Backend("wake count does not fit in usize".into()))
    }

    async fn next_due(&self, now: Timestamp) -> Result<Option<Timestamp>, StoreError> {
        let now = timestamp(now)?;
        // `query_scalar` decodes a SQL NULL as None, so the Rust type is already optional.
        let due: Option<OffsetDateTime> = sqlx::query_scalar(
            "SELECT MIN(wake_at) FROM workflow_instance \
             WHERE wake_at > $1 \
               AND (status = $2 OR (status = $3 AND wait_reason = $4)) \
               AND (lease_until IS NULL OR lease_until <= $1)",
        )
        .bind(now)
        .bind(STATUS_RUNNABLE)
        .bind(STATUS_WAITING)
        .bind(WAIT_TIMER)
        .fetch_one(&self.pool)
        .await
        .map_err(backend)?;
        match due {
            Some(due) => Ok(Some(from_timestamp(due)?)),
            None => Ok(None),
        }
    }
}

async fn apply_children(
    tx: &mut Transaction<'_, Postgres>,
    commit: &Commit,
    at: OffsetDateTime,
) -> Result<(), StoreError> {
    if commit.spawn_children.is_empty() {
        return Ok(());
    }
    let mut seen = std::collections::HashSet::new();
    for child in &commit.spawn_children {
        if !seen.insert(&child.id) {
            return Err(StoreError::AlreadyExists(child.id.clone()));
        }
    }
    let mut ids = Vec::with_capacity(commit.spawn_children.len());
    let mut namespaces = Vec::with_capacity(commit.spawn_children.len());
    let mut names = Vec::with_capacity(commit.spawn_children.len());
    let mut versions = Vec::with_capacity(commit.spawn_children.len());
    let mut hashes = Vec::with_capacity(commit.spawn_children.len());
    let mut parents = Vec::with_capacity(commit.spawn_children.len());
    let mut branches = Vec::with_capacity(commit.spawn_children.len());
    let mut roots = Vec::with_capacity(commit.spawn_children.len());
    let mut inputs = Vec::with_capacity(commit.spawn_children.len());
    for child in &commit.spawn_children {
        ids.push(parse_id(&child.id)?);
        namespaces.push(child.definition.namespace.clone());
        names.push(child.definition.name.clone());
        versions.push(child.definition.version.clone());
        hashes.push(child.definition.content_hash.clone());
        parents.push(parse_id(&child.parent.id)?);
        branches.push(
            child
                .parent
                .branch
                .map(|branch| {
                    i32::try_from(branch).map_err(|_| {
                        StoreError::Backend(format!(
                            "branch index {branch} does not fit in an integer"
                        ))
                    })
                })
                .transpose()?,
        );
        roots.push(child.root_task.clone());
        inputs.push(Json(child.input.clone()));
    }
    // One statement for the whole batch. A duplicate id in the batch was
    // rejected above; ON CONFLICT catches an id that already exists.
    let inserted = sqlx::query(&format!(
        "INSERT INTO workflow_instance ( \
            id, created_at, updated_at, definition_ns, definition_name, \
            definition_version, definition_hash, parent_id, parent_branch, \
            root_task, status, wait_reason, row_version, wake_at, input_json \
         ) \
         SELECT id, $2, $2, namespace, name, version, hash, parent, branch, root, \
                {STATUS_RUNNABLE}, NULL, 0, $2, input \
         FROM UNNEST($1::uuid[], $3::text[], $4::text[], $5::text[], $6::text[], \
                     $7::uuid[], $8::int[], $9::text[], $10::jsonb[]) \
              AS row(id, namespace, name, version, hash, parent, branch, root, input) \
         ON CONFLICT (id) DO NOTHING \
         RETURNING id"
    ))
    .bind(&ids)
    .bind(at)
    .bind(&namespaces)
    .bind(&names)
    .bind(&versions)
    .bind(&hashes)
    .bind(&parents)
    .bind(&branches)
    .bind(&roots)
    .bind(&inputs)
    .fetch_all(&mut **tx)
    .await
    .map_err(backend)?;
    if inserted.len() != commit.spawn_children.len() {
        let written: std::collections::HashSet<Uuid> = inserted
            .into_iter()
            .map(|row| row.try_get::<Uuid, _>("id"))
            .collect::<Result<_, _>>()
            .map_err(backend)?;
        let duplicate = commit
            .spawn_children
            .iter()
            .zip(&ids)
            .find(|(_, id)| !written.contains(id))
            .map(|(child, _)| child.id.clone());
        return Err(match duplicate {
            Some(id) => StoreError::AlreadyExists(id),
            None => StoreError::Backend("spawn inserted fewer rows than requested".into()),
        });
    }
    Ok(())
}

async fn apply_cancels(
    tx: &mut Transaction<'_, Postgres>,
    commit: &Commit,
    at: OffsetDateTime,
) -> Result<(), StoreError> {
    for id in &commit.cancel {
        let uuid = parse_id(id)?;
        // A child spawned in this same commit is visible to this statement.
        let found = sqlx::query_scalar::<_, i32>("SELECT 1 FROM workflow_instance WHERE id = $1")
            .bind(uuid)
            .fetch_optional(&mut **tx)
            .await
            .map_err(backend)?;
        if found.is_none() {
            return Err(StoreError::NotFound(id.clone()));
        }
        sqlx::query(
            "UPDATE workflow_instance \
             SET status = $2, wait_reason = NULL, continuation_json = NULL, \
                 wake_at = NULL, lease_worker = NULL, lease_until = NULL, \
                 row_version = row_version + 1, updated_at = $3 \
             WHERE id = $1 AND status NOT IN ($4, $5, $6)",
        )
        .bind(uuid)
        .bind(STATUS_CANCELLED)
        .bind(at)
        .bind(STATUS_COMPLETED)
        .bind(STATUS_FAULTED)
        .bind(STATUS_CANCELLED)
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    }
    Ok(())
}

async fn apply_steps(
    tx: &mut Transaction<'_, Postgres>,
    commit: &Commit,
    at: OffsetDateTime,
) -> Result<(), StoreError> {
    for step in &commit.step_results {
        let instance = parse_id(&step.instance)?;
        let (outcome, result) = encode_outcome(&step.outcome)?;
        let attempt = i32::try_from(step.attempt).map_err(|_| {
            StoreError::Backend(format!(
                "attempt {} does not fit in an integer",
                step.attempt
            ))
        })?;
        sqlx::query(
            "INSERT INTO task_execution ( \
                created_at, updated_at, instance_id, position, execution_key, attempt, \
                kind, outcome, input_json, result_json, started_at, finished_at \
             ) VALUES ($1, $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
             ON CONFLICT (instance_id, position, execution_key, attempt) \
             DO UPDATE SET outcome = EXCLUDED.outcome, result_json = EXCLUDED.result_json, \
                           finished_at = EXCLUDED.finished_at, updated_at = EXCLUDED.updated_at",
        )
        .bind(at)
        .bind(instance)
        .bind(step.key.position())
        .bind(step.key.execution_key().to_string())
        .bind(attempt)
        .bind(encode_kind(step.kind)?)
        .bind(outcome)
        .bind(Json(step.input.clone()))
        .bind(result.map(Json))
        .bind(timestamp(step.started_at)?)
        .bind(timestamp(step.finished_at)?)
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    }
    Ok(())
}

fn returning_columns() -> String {
    INSTANCE_COLUMNS
        .split(", ")
        .map(|column| format!("i.{column}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, sqlx::FromRow)]
struct InstanceRow {
    id: Uuid,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    definition_ns: String,
    definition_name: String,
    definition_version: String,
    definition_hash: String,
    parent_id: Option<Uuid>,
    parent_branch: Option<i32>,
    root_task: Option<String>,
    status: i16,
    wait_reason: Option<i16>,
    row_version: i64,
    lease_worker: Option<String>,
    lease_until: Option<OffsetDateTime>,
    in_flight_key: Option<String>,
    in_flight_attempt: Option<i32>,
    wake_at: Option<OffsetDateTime>,
    continuation_json: Option<Json<Value>>,
    input_json: Json<Value>,
    output_json: Option<Json<Value>>,
    fault_json: Option<Json<FaultJson>>,
}

impl TryFrom<Instance> for InstanceRow {
    type Error = StoreError;

    fn try_from(instance: Instance) -> Result<Self, Self::Error> {
        let (status, wait_reason) = encode_status(instance.status)?;
        let in_flight = encode_in_flight(instance.in_flight.as_ref())?;
        let parent = match instance.parent {
            Some(parent) => Some((parse_id(&parent.id)?, encode_branch(parent.branch)?)),
            None => None,
        };
        Ok(Self {
            id: parse_id(&instance.id)?,
            created_at: timestamp(instance.created_at)?,
            updated_at: timestamp(instance.updated_at)?,
            definition_ns: instance.definition.namespace,
            definition_name: instance.definition.name,
            definition_version: instance.definition.version,
            definition_hash: instance.definition.content_hash,
            parent_id: parent.as_ref().map(|value| value.0),
            parent_branch: parent.as_ref().and_then(|value| value.1),
            root_task: instance.root_task,
            status,
            wait_reason,
            row_version: i64::try_from(instance.version).map_err(|_| {
                StoreError::Backend(format!(
                    "version {} does not fit in a bigint",
                    instance.version
                ))
            })?,
            lease_worker: instance.lease.as_ref().map(|lease| lease.worker.0.clone()),
            lease_until: instance
                .lease
                .as_ref()
                .map(|lease| timestamp(lease.until))
                .transpose()?,
            in_flight_key: in_flight.as_ref().map(|value| value.0.clone()),
            in_flight_attempt: in_flight.as_ref().map(|value| value.1),
            wake_at: instance.wake_at.map(timestamp).transpose()?,
            continuation_json: encode_continuation(instance.continuation.as_deref())?,
            input_json: Json(instance.input),
            output_json: instance.output.map(Json),
            fault_json: instance.fault.as_ref().map(encode_fault).map(Json),
        })
    }
}

impl TryFrom<InstanceRow> for Instance {
    type Error = StoreError;

    fn try_from(row: InstanceRow) -> Result<Self, Self::Error> {
        let parent = match row.parent_id {
            Some(id) => Some(ParentRef {
                id: InstanceId::from_string(id.to_string()),
                branch: decode_branch(row.parent_branch)?,
            }),
            None => None,
        };
        Ok(Self {
            id: InstanceId::from_string(row.id.to_string()),
            definition: DefinitionKey {
                namespace: row.definition_ns,
                name: row.definition_name,
                version: row.definition_version,
                content_hash: row.definition_hash,
            },
            parent,
            status: decode_status(row.status, row.wait_reason)?,
            continuation: decode_continuation(row.continuation_json)?,
            version: u64::try_from(row.row_version).map_err(|_| {
                StoreError::Backend(format!("stored version {} is negative", row.row_version))
            })?,
            lease: match (row.lease_worker, row.lease_until) {
                (Some(worker), Some(until)) => Some(Lease {
                    worker: WorkerId(worker),
                    until: from_timestamp(until)?,
                }),
                (None, None) => None,
                _ => {
                    return Err(StoreError::Backend(format!(
                        "instance {} has a partial lease",
                        row.id
                    )));
                }
            },
            wake_at: row.wake_at.map(from_timestamp).transpose()?,
            root_task: row.root_task,
            in_flight: decode_in_flight(row.in_flight_key, row.in_flight_attempt)?,
            input: row.input_json.0,
            output: row.output_json.map(|value| value.0),
            fault: row.fault_json.map(|value| value.0.into()),
            created_at: from_timestamp(row.created_at)?,
            updated_at: from_timestamp(row.updated_at)?,
        })
    }
}

#[derive(Debug, sqlx::FromRow)]
struct StepRow {
    instance_id: Uuid,
    position: String,
    execution_key: String,
    attempt: i32,
    kind: i16,
    outcome: i16,
    input_json: Json<Value>,
    result_json: Option<Json<Value>>,
    started_at: OffsetDateTime,
    finished_at: OffsetDateTime,
}

impl TryFrom<StepRow> for StepResult {
    type Error = StoreError;

    fn try_from(row: StepRow) -> Result<Self, Self::Error> {
        Ok(Self {
            instance: InstanceId::from_string(row.instance_id.to_string()),
            key: TaskKey::new(row.position, parse_execution_key(&row.execution_key)?),
            kind: decode_kind(row.kind)?,
            attempt: u32::try_from(row.attempt).map_err(|_| {
                StoreError::Backend(format!("stored attempt {} is negative", row.attempt))
            })?,
            input: row.input_json.0,
            outcome: decode_outcome(row.outcome, row.result_json)?,
            started_at: from_timestamp(row.started_at)?,
            finished_at: from_timestamp(row.finished_at)?,
        })
    }
}

/// RFC 7807 fields, stored as JSON. `Fault` keeps its fields private.
#[derive(Debug, Serialize, Deserialize)]
struct FaultJson {
    #[serde(rename = "type")]
    error_type: String,
    status: u16,
    instance: String,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl From<FaultJson> for Fault {
    fn from(value: FaultJson) -> Self {
        // Reconstruct through the public constructors when the type matches,
        // then overwrite nothing: constructors fix status and title. A stored
        // fault that does not match a known type is still a runtime fault of
        // the same instance and detail, which is the only public builder that
        // accepts an arbitrary message. Known types round-trip exactly.
        match value.error_type.as_str() {
            ty if ty.ends_with("/errors/expression") => {
                Fault::expression(value.instance, value.detail.unwrap_or_default())
            }
            ty if ty.ends_with("/errors/runtime") => {
                Fault::runtime(value.instance, value.detail.unwrap_or_default())
            }
            ty if ty.ends_with("/errors/configuration") => {
                Fault::configuration(value.instance, value.detail.unwrap_or_default())
            }
            ty if ty.ends_with("/errors/validation") => {
                Fault::validation(value.instance, value.detail.unwrap_or_default())
            }
            ty if ty.ends_with("/errors/authentication") => {
                Fault::authentication(value.instance, value.detail.unwrap_or_default())
            }
            ty if ty.ends_with("/errors/authorization") => {
                Fault::authorization(value.instance, value.detail.unwrap_or_default())
            }
            ty if ty.ends_with("/errors/timeout") => {
                Fault::timeout(value.instance, value.detail.unwrap_or_default())
            }
            ty if ty.ends_with("/errors/communication") => Fault::communication(
                value.instance,
                value.status,
                value.detail.unwrap_or_default(),
            ),
            _ => Fault::runtime(value.instance, value.detail.unwrap_or(value.title)),
        }
    }
}

fn encode_fault(fault: &Fault) -> FaultJson {
    FaultJson {
        error_type: fault.error_type().to_string(),
        status: fault.status(),
        instance: fault.instance().to_string(),
        title: fault.title().to_string(),
        detail: fault.detail().map(str::to_string),
    }
}

fn encode_status(status: Status) -> Result<(i16, Option<i16>), StoreError> {
    Ok(match status {
        Status::Runnable => (STATUS_RUNNABLE, None),
        Status::Running => (STATUS_RUNNING, None),
        Status::Waiting(WaitReason::Timer) => (STATUS_WAITING, Some(WAIT_TIMER)),
        Status::Waiting(WaitReason::Join) => (STATUS_WAITING, Some(WAIT_JOIN)),
        Status::Waiting(WaitReason::Child) => (STATUS_WAITING, Some(WAIT_CHILD)),
        Status::Waiting(WaitReason::Events) => (STATUS_WAITING, Some(WAIT_EVENTS)),
        Status::Completed => (STATUS_COMPLETED, None),
        Status::Faulted => (STATUS_FAULTED, None),
        Status::Cancelled => (STATUS_CANCELLED, None),
    })
}

fn decode_status(status: i16, wait_reason: Option<i16>) -> Result<Status, StoreError> {
    match (status, wait_reason) {
        (STATUS_RUNNABLE, None) => Ok(Status::Runnable),
        (STATUS_RUNNING, None) => Ok(Status::Running),
        (STATUS_WAITING, Some(WAIT_TIMER)) => Ok(Status::Waiting(WaitReason::Timer)),
        (STATUS_WAITING, Some(WAIT_JOIN)) => Ok(Status::Waiting(WaitReason::Join)),
        (STATUS_WAITING, Some(WAIT_CHILD)) => Ok(Status::Waiting(WaitReason::Child)),
        (STATUS_WAITING, Some(WAIT_EVENTS)) => Ok(Status::Waiting(WaitReason::Events)),
        (STATUS_COMPLETED, None) => Ok(Status::Completed),
        (STATUS_FAULTED, None) => Ok(Status::Faulted),
        (STATUS_CANCELLED, None) => Ok(Status::Cancelled),
        _ => Err(StoreError::Backend(format!(
            "unknown status {status} wait_reason {wait_reason:?}"
        ))),
    }
}

fn encode_kind(kind: TaskKind) -> Result<i16, StoreError> {
    Ok(match kind {
        TaskKind::Http => KIND_HTTP,
        TaskKind::Grpc => KIND_GRPC,
        TaskKind::OpenApi => KIND_OPENAPI,
        TaskKind::AsyncApi => KIND_ASYNCAPI,
        TaskKind::A2a => KIND_A2A,
        TaskKind::Mcp => KIND_MCP,
        TaskKind::Emit => KIND_EMIT,
        TaskKind::Shell => KIND_SHELL,
        TaskKind::Script => KIND_SCRIPT,
        TaskKind::Container => KIND_CONTAINER,
    })
}

fn decode_kind(kind: i16) -> Result<TaskKind, StoreError> {
    match kind {
        KIND_HTTP => Ok(TaskKind::Http),
        KIND_GRPC => Ok(TaskKind::Grpc),
        KIND_OPENAPI => Ok(TaskKind::OpenApi),
        KIND_ASYNCAPI => Ok(TaskKind::AsyncApi),
        KIND_A2A => Ok(TaskKind::A2a),
        KIND_MCP => Ok(TaskKind::Mcp),
        KIND_EMIT => Ok(TaskKind::Emit),
        KIND_SHELL => Ok(TaskKind::Shell),
        KIND_SCRIPT => Ok(TaskKind::Script),
        KIND_CONTAINER => Ok(TaskKind::Container),
        _ => Err(StoreError::Backend(format!("unknown task kind {kind}"))),
    }
}

fn encode_outcome(outcome: &StepOutcome) -> Result<(i16, Option<Value>), StoreError> {
    Ok(match outcome {
        StepOutcome::Pending => (OUTCOME_PENDING, None),
        StepOutcome::Output(value) => (OUTCOME_OUTPUT, Some(value.clone())),
        StepOutcome::Fault(fault) => (OUTCOME_FAULT, Some(fault.to_value())),
        StepOutcome::Unknown => (OUTCOME_UNKNOWN, None),
    })
}

fn decode_outcome(outcome: i16, result: Option<Json<Value>>) -> Result<StepOutcome, StoreError> {
    match (outcome, result) {
        (OUTCOME_PENDING, _) => Ok(StepOutcome::Pending),
        (OUTCOME_OUTPUT, Some(value)) => Ok(StepOutcome::Output(value.0)),
        (OUTCOME_FAULT, Some(value)) => {
            let stored: FaultJson = serde_json::from_value(value.0).map_err(backend)?;
            Ok(StepOutcome::Fault(stored.into()))
        }
        (OUTCOME_UNKNOWN, _) => Ok(StepOutcome::Unknown),
        _ => Err(StoreError::Backend(format!(
            "outcome {outcome} is missing its result"
        ))),
    }
}

fn encode_in_flight(in_flight: Option<&InFlight>) -> Result<Option<(String, i32)>, StoreError> {
    let Some(in_flight) = in_flight else {
        return Ok(None);
    };
    let attempt = i32::try_from(in_flight.attempt).map_err(|_| {
        StoreError::Backend(format!(
            "in-flight attempt {} does not fit in an integer",
            in_flight.attempt
        ))
    })?;
    Ok(Some((in_flight.key.to_string(), attempt)))
}

fn decode_in_flight(
    key: Option<String>,
    attempt: Option<i32>,
) -> Result<Option<InFlight>, StoreError> {
    match (key, attempt) {
        (None, None) => Ok(None),
        (Some(key), Some(attempt)) => Ok(Some(InFlight {
            key: parse_task_key(&key)?,
            attempt: u32::try_from(attempt).map_err(|_| {
                StoreError::Backend(format!("stored in-flight attempt {attempt} is negative"))
            })?,
        })),
        _ => Err(StoreError::Backend(
            "in-flight key and attempt must both be set or both be null".into(),
        )),
    }
}

fn encode_branch(branch: Option<usize>) -> Result<Option<i32>, StoreError> {
    branch
        .map(|branch| {
            i32::try_from(branch).map_err(|_| {
                StoreError::Backend(format!("branch index {branch} does not fit in an integer"))
            })
        })
        .transpose()
}

fn decode_branch(branch: Option<i32>) -> Result<Option<usize>, StoreError> {
    branch
        .map(|branch| {
            usize::try_from(branch).map_err(|_| {
                StoreError::Backend(format!("stored branch index {branch} is negative"))
            })
        })
        .transpose()
}

fn encode_continuation(bytes: Option<&[u8]>) -> Result<Option<Json<Value>>, StoreError> {
    bytes
        .map(|bytes| {
            Ok(Json(Value::Array(
                bytes.iter().map(|byte| Value::from(*byte)).collect(),
            )))
        })
        .transpose()
}

fn decode_continuation(value: Option<Json<Value>>) -> Result<Option<Vec<u8>>, StoreError> {
    let Some(Json(value)) = value else {
        return Ok(None);
    };
    let Value::Array(items) = value else {
        return Err(StoreError::Backend(
            "continuation_json is not a byte array".into(),
        ));
    };
    let mut bytes = Vec::with_capacity(items.len());
    for item in items {
        let Value::Number(number) = item else {
            return Err(StoreError::Backend(
                "continuation_json contains a non-number".into(),
            ));
        };
        let byte = number
            .as_u64()
            .and_then(|value| u8::try_from(value).ok())
            .ok_or_else(|| {
                StoreError::Backend("continuation_json contains a byte outside 0..=255".into())
            })?;
        bytes.push(byte);
    }
    Ok(Some(bytes))
}

/// `TaskKey`'s display form: `position` or `position@loop:n/attempt:n/pass:n`.
fn parse_task_key(text: &str) -> Result<TaskKey, StoreError> {
    let Some((position, key)) = text.rsplit_once('@') else {
        return Ok(TaskKey::new(text, ExecutionKey::empty()));
    };
    // A JSON Pointer never contains `@`. An execution key always starts with
    // a counter name, so a trailing `@` with no counters is rejected below.
    if position.contains('@') {
        return Err(StoreError::Backend(format!(
            "in-flight key {text:?} has more than one '@'"
        )));
    }
    Ok(TaskKey::new(position, parse_execution_key(key)?))
}

fn parse_execution_key(text: &str) -> Result<ExecutionKey, StoreError> {
    if text.is_empty() {
        return Ok(ExecutionKey::empty());
    }
    let mut counters = Vec::new();
    for part in text.split('/') {
        let (name, value) = part.split_once(':').ok_or_else(|| {
            StoreError::Backend(format!("execution key {text:?} has a counter without ':'"))
        })?;
        let value = value.parse::<u64>().map_err(|_| {
            StoreError::Backend(format!("execution key {text:?} has a non-numeric counter"))
        })?;
        let counter = match name {
            "loop" => Counter::Loop(value),
            "attempt" => Counter::Attempt(value),
            "pass" => Counter::Pass(value),
            _ => {
                return Err(StoreError::Backend(format!(
                    "execution key {text:?} has an unknown counter {name}"
                )));
            }
        };
        counters.push(counter);
    }
    Ok(ExecutionKey::new(counters))
}

fn parse_id(id: &InstanceId) -> Result<Uuid, StoreError> {
    Uuid::from_str(id.as_str()).map_err(|error| {
        StoreError::Backend(format!(
            "instance id {} is not a UUID: {error}",
            id.as_str()
        ))
    })
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

fn from_timestamp(value: OffsetDateTime) -> Result<Timestamp, StoreError> {
    let millis = value.unix_timestamp_nanos() / 1_000_000;
    let millis = u64::try_from(millis).map_err(|_| {
        StoreError::Backend(format!("stored timestamp {value} is before the Unix epoch"))
    })?;
    Ok(Timestamp::from_millis(millis))
}

fn backend(error: impl ToString) -> StoreError {
    StoreError::Backend(error.to_string())
}
