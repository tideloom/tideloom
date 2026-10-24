//! PostgreSQL [`EventSink`](tideloom_runtime::EventSink) and
//! [`EventSource`](tideloom_runtime::EventSource).
//!
//! A published event is copied into every inbox whose [`ListenSpec`] already
//! matches it. An event published before [`EventSource::subscribe`] is lost;
//! this bus does not keep a log to replay into a later inbox. The inbox
//! outlives the claim; only [`EventSource::ack`] removes events from a read.
//!
//! Matching is string equality on CloudEvents context attributes, the same
//! rule as [`LocalEventBus`](tideloom_runtime::LocalEventBus). `data` is not a
//! filter. One filter matches only when every one of its attributes equals
//! the event's. `foreach`, `until`, and correlation are rejected by the core
//! before a spec reaches this bus.
//!
//! Time never comes from the database clock. [`PgEventBus::publish_at`] and
//! [`PgEventBus::subscribe_at`] take the caller's [`Timestamp`]. Trait methods
//! use the bus clock, which tests can replace with [`PgEventBus::with_clock`].

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Json;
use sqlx::types::Uuid;
use sqlx::types::time::OffsetDateTime;
use tideloom_core::EventFilter;
use tideloom_core::ListenSpec;
use tideloom_core::Timestamp;
use tideloom_runtime::Clock;
use tideloom_runtime::CloudEvent;
use tideloom_runtime::EventError;
use tideloom_runtime::EventSink;
use tideloom_runtime::EventSource;
use tideloom_runtime::InstanceId;
use tideloom_runtime::SystemClock;

/// `workflow_instance.status`.
const STATUS_RUNNABLE: i16 = 0;
const STATUS_WAITING: i16 = 2;

/// `workflow_instance.wait_reason` for an events wait.
const WAIT_EVENTS: i16 = 3;

const ACTIVE: i16 = 1;
const INACTIVE: i16 = 0;
const UNACKED: i16 = 0;
const ACKED: i16 = 1;

/// Event subscriptions and inboxes in PostgreSQL.
///
/// A pool is shared across workers in one process. `publish` inserts inbox
/// rows with `ON CONFLICT DO NOTHING`, so a repeated delivery of the same
/// event id does not write a second row and does not appear in the returned
/// instance ids.
#[derive(Clone)]
pub struct PgEventBus {
    pool: PgPool,
    /// When set, `publish` also flips unarmed `Waiting(Events)` rows in the
    /// same transaction as the inbox insert. A later `InstanceStore::wake`
    /// sees `wake_at` already set and matches nothing.
    wake: bool,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for PgEventBus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgEventBus")
            .field("pool", &self.pool)
            .field("wake", &self.wake)
            .finish_non_exhaustive()
    }
}

impl PgEventBus {
    /// Open a pool. Does not migrate; call [`PgStore::migrate`](crate::PgStore::migrate)
    /// once per database. Trait method timestamps come from [`SystemClock`].
    ///
    /// # Errors
    ///
    /// Returns [`EventError::Backend`] when the URL is rejected or the pool
    /// cannot connect.
    pub async fn connect(url: &str) -> Result<Self, EventError> {
        Self::connect_inner(url, false).await
    }

    /// A bus that, inside `publish`'s transaction, also runs the same
    /// conditional update as `PgStore`'s `wake`.
    ///
    /// Only a `Waiting(Events)` row whose `wake_at` is null flips to
    /// `Runnable` with `wake_at` set to the publish time. A second wake of
    /// the same row, from this bus or from the store, matches nothing.
    ///
    /// # Errors
    ///
    /// Returns [`EventError::Backend`] when the URL is rejected or the pool
    /// cannot connect.
    pub async fn connect_waking(url: &str) -> Result<Self, EventError> {
        Self::connect_inner(url, true).await
    }

    async fn connect_inner(url: &str, wake: bool) -> Result<Self, EventError> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(url)
            .await
            .map_err(backend)?;
        Ok(Self {
            pool,
            wake,
            clock: Arc::new(SystemClock),
        })
    }

    /// Record timestamps from `clock` instead of [`SystemClock`].
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The pool, for tests that read subscription rows directly.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// [`EventSink::publish`] at `now`.
    ///
    /// `now` is the `created_at` of a newly inserted inbox row, and the
    /// `wake_at` written when this bus was opened with
    /// [`PgEventBus::connect_waking`]. It is not read from the database.
    ///
    /// # Errors
    ///
    /// Returns [`EventError::Backend`] when a statement fails or an instance
    /// id stored as text is not a UUID.
    pub async fn publish_at(
        &self,
        event: CloudEvent,
        now: Timestamp,
    ) -> Result<Vec<InstanceId>, EventError> {
        let now = timestamp(now)?;
        let attributes = event_attributes(&event);
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let candidates = candidate_subscriptions(&mut tx, &attributes).await?;
        let mut landed = Vec::new();
        let event_json = Json(serde_json::to_value(&event).map_err(backend)?);
        for (instance, filters) in candidates {
            if !filters.iter().any(|filter| matches_filter(filter, &event)) {
                continue;
            }
            // Lock the subscription before inserting. `unsubscribe` locks the
            // same row, deletes the inbox, then commits. An insert that only
            // checked the candidate snapshot could land after that delete and
            // refill an inbox the listener had already dropped. The `WHERE`
            // re-reads `is_active` under that lock, so a deactivated row
            // inserts nothing.
            let inserted = sqlx::query(
                "INSERT INTO event_inbox \
                 (created_at, updated_at, instance_id, event_id, event_json, is_acked) \
                 SELECT $1, $1, s.instance_id, $3, $4, $5 \
                 FROM event_subscription AS s \
                 WHERE s.instance_id = $2 AND s.is_active = $6 \
                 FOR UPDATE OF s \
                 ON CONFLICT (instance_id, event_id) DO NOTHING",
            )
            .bind(now)
            .bind(instance)
            .bind(&event.id)
            .bind(&event_json)
            .bind(UNACKED)
            .bind(ACTIVE)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
            if inserted.rows_affected() == 1 {
                landed.push(InstanceId::from_string(instance.to_string()));
            }
        }
        if self.wake && !landed.is_empty() {
            let uuids = landed.iter().map(parse_id).collect::<Result<Vec<_>, _>>()?;
            sqlx::query(
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
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        }
        tx.commit().await.map_err(backend)?;
        Ok(landed)
    }

    /// [`EventSource::subscribe`] at `now`.
    ///
    /// A repeated call with the same spec writes nothing. A different spec
    /// replaces the attribute rows and keeps the inbox. Events published
    /// before this call are not copied in. `now` stamps new or replaced rows;
    /// it is not read from the database.
    ///
    /// # Errors
    ///
    /// Returns [`EventError::Backend`] when a statement fails or `instance`
    /// is not a UUID.
    pub async fn subscribe_at(
        &self,
        instance: &InstanceId,
        spec: ListenSpec,
        now: Timestamp,
    ) -> Result<(), EventError> {
        let instance_id = parse_id(instance)?;
        let now = timestamp(now)?;
        let spec_json = Json(serde_json::to_value(&spec).map_err(backend)?);
        let mut tx = self.pool.begin().await.map_err(backend)?;
        // Lock before reading the spec. `publish` locks the same row before
        // it inserts, so a spec replacement cannot land an event under the
        // spec this transaction is about to delete.
        let existing = sqlx::query_as::<_, (i64, Json<Value>)>(
            "SELECT id, spec_json FROM event_subscription \
             WHERE instance_id = $1 AND is_active = $2 \
             FOR UPDATE",
        )
        .bind(instance_id)
        .bind(ACTIVE)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        if let Some((id, stored)) = existing {
            let stored_spec: ListenSpec = serde_json::from_value(stored.0).map_err(backend)?;
            if stored_spec == spec {
                tx.commit().await.map_err(backend)?;
                return Ok(());
            }
            sqlx::query("DELETE FROM event_subscription_attr WHERE subscription_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
            sqlx::query(
                "UPDATE event_subscription SET spec_json = $2, updated_at = $3 WHERE id = $1",
            )
            .bind(id)
            .bind(&spec_json)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
            insert_attrs(&mut tx, id, &spec, now).await?;
            tx.commit().await.map_err(backend)?;
            return Ok(());
        }
        // A deactivated row keeps its instance id, which is unique. Reactivate
        // it instead of inserting a second row. Its inbox was deleted on
        // unsubscribe, and nothing published while it was inactive is copied in.
        let reactivated = sqlx::query_scalar::<_, i64>(
            "UPDATE event_subscription \
             SET spec_json = $2, is_active = $3, updated_at = $4 \
             WHERE instance_id = $1 \
             RETURNING id",
        )
        .bind(instance_id)
        .bind(&spec_json)
        .bind(ACTIVE)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        let subscription_id = if let Some(id) = reactivated {
            sqlx::query("DELETE FROM event_subscription_attr WHERE subscription_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
            id
        } else {
            sqlx::query_scalar::<_, i64>(
                "INSERT INTO event_subscription \
                 (created_at, updated_at, instance_id, spec_json, is_active) \
                 VALUES ($1, $1, $2, $3, $4) \
                 RETURNING id",
            )
            .bind(now)
            .bind(instance_id)
            .bind(&spec_json)
            .bind(ACTIVE)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?
        };
        insert_attrs(&mut tx, subscription_id, &spec, now).await?;
        tx.commit().await.map_err(backend)?;
        Ok(())
    }

    /// [`EventSource::unsubscribe`] at `now`.
    ///
    /// Marks the subscription inactive and deletes the inbox. `now` is the
    /// row's new `updated_at`. An instance that was never subscribed is a
    /// no-op.
    ///
    /// # Errors
    ///
    /// Returns [`EventError::Backend`] when a statement fails or `instance`
    /// is not a UUID.
    pub async fn unsubscribe_at(
        &self,
        instance: &InstanceId,
        now: Timestamp,
    ) -> Result<(), EventError> {
        let instance_id = parse_id(instance)?;
        let now = timestamp(now)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        // `FOR UPDATE` is the same row `publish` locks before inserting. A
        // publish that has not locked it yet sees `is_active = 0` and inserts
        // nothing. A publish that already holds it waits, then the `is_active`
        // predicate rejects the insert, so the delete below is not undone.
        sqlx::query(
            "UPDATE event_subscription SET is_active = $2, updated_at = $4 \
             WHERE instance_id = $1 AND is_active = $3 \
             RETURNING id",
        )
        .bind(instance_id)
        .bind(INACTIVE)
        .bind(ACTIVE)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        sqlx::query("DELETE FROM event_inbox WHERE instance_id = $1")
            .bind(instance_id)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(())
    }
}

#[async_trait]
impl EventSink for PgEventBus {
    async fn publish(&self, event: CloudEvent) -> Result<Vec<InstanceId>, EventError> {
        self.publish_at(event, self.clock.now()).await
    }
}

#[async_trait]
impl EventSource for PgEventBus {
    async fn subscribe(&self, instance: &InstanceId, spec: ListenSpec) -> Result<(), EventError> {
        self.subscribe_at(instance, spec, self.clock.now()).await
    }

    async fn unsubscribe(&self, instance: &InstanceId) -> Result<(), EventError> {
        self.unsubscribe_at(instance, self.clock.now()).await
    }

    async fn inbox(&self, instance: &InstanceId) -> Result<Vec<CloudEvent>, EventError> {
        let instance_id = parse_id(instance)?;
        let rows = sqlx::query_scalar::<_, Json<Value>>(
            "SELECT event_json FROM event_inbox \
             WHERE instance_id = $1 AND is_acked = $2 \
             ORDER BY id",
        )
        .bind(instance_id)
        .bind(UNACKED)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter()
            .map(|Json(value)| serde_json::from_value(value).map_err(backend))
            .collect()
    }

    async fn ack(&self, instance: &InstanceId, ids: &[String]) -> Result<(), EventError> {
        if ids.is_empty() {
            return Ok(());
        }
        let instance_id = parse_id(instance)?;
        let now = timestamp(self.clock.now())?;
        sqlx::query(
            "UPDATE event_inbox SET updated_at = $3, is_acked = $4 \
             WHERE instance_id = $1 AND event_id = ANY($2) AND is_acked = $5",
        )
        .bind(instance_id)
        .bind(ids)
        .bind(now)
        .bind(ACKED)
        .bind(UNACKED)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}

async fn candidate_subscriptions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attributes: &[(String, String)],
) -> Result<Vec<(Uuid, Vec<EventFilter>)>, EventError> {
    if attributes.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<&str> = attributes.iter().map(|(name, _)| name.as_str()).collect();
    let values: Vec<&str> = attributes.iter().map(|(_, value)| value.as_str()).collect();
    let rows = sqlx::query_as::<_, (Uuid, i32, String, String)>(
        "SELECT s.instance_id, a.filter_index, a.attr_name, a.attr_value \
         FROM event_subscription AS s \
         JOIN event_subscription_attr AS a ON a.subscription_id = s.id \
         WHERE s.is_active = $1 \
           AND s.id IN ( \
               SELECT hit.subscription_id \
               FROM event_subscription_attr AS hit \
               JOIN event_subscription AS owner ON owner.id = hit.subscription_id \
               WHERE owner.is_active = $1 \
                 AND (hit.attr_name, hit.attr_value) IN ( \
                     SELECT * FROM UNNEST($2::text[], $3::text[]) \
                 ) \
           ) \
         ORDER BY s.instance_id, a.filter_index, a.attr_name",
    )
    .bind(ACTIVE)
    .bind(&names)
    .bind(&values)
    .fetch_all(&mut **tx)
    .await
    .map_err(backend)?;
    Ok(group_filters(rows))
}

fn group_filters(rows: Vec<(Uuid, i32, String, String)>) -> Vec<(Uuid, Vec<EventFilter>)> {
    let mut grouped: BTreeMap<Uuid, BTreeMap<i32, BTreeMap<String, String>>> = BTreeMap::new();
    for (instance, index, name, value) in rows {
        grouped
            .entry(instance)
            .or_default()
            .entry(index)
            .or_default()
            .insert(name, value);
    }
    grouped
        .into_iter()
        .map(|(instance, filters)| {
            let filters = filters
                .into_values()
                .map(|attributes| EventFilter { attributes })
                .collect();
            (instance, filters)
        })
        .collect()
}

async fn insert_attrs(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    subscription_id: i64,
    spec: &ListenSpec,
    now: OffsetDateTime,
) -> Result<(), EventError> {
    for (index, filter) in spec.filters.iter().enumerate() {
        let filter_index = i32::try_from(index).map_err(|_| {
            EventError::Backend(format!("filter index {index} does not fit in an integer"))
        })?;
        for (name, value) in &filter.attributes {
            sqlx::query(
                "INSERT INTO event_subscription_attr \
                 (created_at, updated_at, subscription_id, filter_index, attr_name, attr_value) \
                 VALUES ($1, $1, $2, $3, $4, $5)",
            )
            .bind(now)
            .bind(subscription_id)
            .bind(filter_index)
            .bind(name)
            .bind(value)
            .execute(&mut **tx)
            .await
            .map_err(backend)?;
        }
    }
    Ok(())
}

fn matches_filter(filter: &EventFilter, event: &CloudEvent) -> bool {
    filter
        .attributes
        .iter()
        .all(|(name, expected)| event.attribute(name) == Some(expected.as_str()))
}

fn event_attributes(event: &CloudEvent) -> Vec<(String, String)> {
    let mut attributes = vec![
        ("id".to_string(), event.id.clone()),
        ("type".to_string(), event.r#type.clone()),
        ("source".to_string(), event.source.clone()),
    ];
    if let Some(subject) = &event.subject {
        attributes.push(("subject".to_string(), subject.clone()));
    }
    if let Some(time) = &event.time {
        attributes.push(("time".to_string(), time.clone()));
    }
    attributes.extend(
        event
            .extensions
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    attributes
}

fn parse_id(id: &InstanceId) -> Result<Uuid, EventError> {
    Uuid::from_str(id.as_str()).map_err(|error| {
        EventError::Backend(format!(
            "instance id {} is not a UUID: {error}",
            id.as_str()
        ))
    })
}

fn timestamp(value: Timestamp) -> Result<OffsetDateTime, EventError> {
    let millis = i128::from(value.millis());
    OffsetDateTime::from_unix_timestamp_nanos(millis * 1_000_000).map_err(|error| {
        EventError::Backend(format!(
            "timestamp {} is out of range: {error}",
            value.millis()
        ))
    })
}

fn backend(error: impl ToString) -> EventError {
    EventError::Backend(error.to_string())
}
