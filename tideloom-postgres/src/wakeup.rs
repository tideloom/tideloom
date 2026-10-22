//! PostgreSQL [`Wakeup`](tideloom_runtime::Wakeup).
//!
//! `signal_log.id` is the offset. `notify` inserts a row and sends
//! `pg_notify` on `tideloom_signal`. `watch` replays `id > from` first, then
//! listens. A lost notification is covered by polling the table every
//! [`PgWakeup::poll_interval`]; the notification is a hint, not the log.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use sqlx::postgres::PgPoolOptions;
use sqlx::types::Uuid;
use sqlx::types::time::OffsetDateTime;
use tideloom_core::Timestamp;
use tideloom_runtime::BusError;
use tideloom_runtime::Clock;
use tideloom_runtime::Hint;
use tideloom_runtime::InstanceId;
use tideloom_runtime::Offset;
use tideloom_runtime::Signal;
use tideloom_runtime::SignalStream;
use tideloom_runtime::SystemClock;
use tideloom_runtime::Wakeup;

/// `pg_notify` channel. The payload is empty; the row is the signal.
const CHANNEL: &str = "tideloom_signal";

/// `signal_log.hint`.
const HINT_TIMER: i16 = 0;
const HINT_CHILD_DONE: i16 = 1;
const HINT_EVENT: i16 = 2;
const HINT_OTHER: i16 = 3;

/// How often a watcher re-reads `signal_log` when no notification arrives.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A replayable signal log in PostgreSQL, shared by every worker on the database.
///
/// One instance serves one consumer group. Other groups are other rows in
/// `signal_cursor`; the trait has no group parameter, so the group lives here.
/// Retention is not this type's job: a `from` older than the oldest retained
/// id returns [`BusError::OffsetExpired`].
#[derive(Clone)]
pub struct PgWakeup {
    pool: PgPool,
    /// Opens the `LISTEN` connection. A listener holds its connection for the
    /// life of `watch`, so it is not taken from [`PgWakeup::pool`].
    listen_url: Option<String>,
    consumer_group: String,
    poll_interval: Duration,
    clock: Arc<dyn Clock>,
}

impl PgWakeup {
    /// Open a pool for `consumer_group`. Does not migrate; call [`PgWakeup::migrate`]
    /// once per database, or [`crate::PgStore::migrate`], which applies the same files.
    ///
    /// # Errors
    ///
    /// Returns [`BusError::Backend`] when the URL is rejected or the pool cannot connect.
    pub async fn connect(url: &str, consumer_group: impl Into<String>) -> Result<Self, BusError> {
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect(url)
            .await
            .map_err(backend)?;
        Ok(Self::new(pool, consumer_group).with_listen_url(url))
    }

    /// A bus on an existing pool. `consumer_group` selects the `signal_cursor` row
    /// this instance reads and writes. Audit timestamps come from [`SystemClock`];
    /// no query compares them. Tests that must not read the wall clock pass
    /// [`PgWakeup::with_clock`].
    ///
    /// `watch` needs a URL for its own `LISTEN` connection, which a listener
    /// holds until the stream ends. [`PgWakeup::connect`] sets it.
    /// [`PgWakeup::with_listen_url`] sets it when the pool already exists.
    /// Without one, `watch` returns [`BusError::Backend`].
    #[must_use]
    pub fn new(pool: PgPool, consumer_group: impl Into<String>) -> Self {
        Self {
            pool,
            listen_url: None,
            consumer_group: consumer_group.into(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            clock: Arc::new(SystemClock),
        }
    }

    /// URL for the dedicated `LISTEN` connection. Not taken from the pool.
    #[must_use]
    pub fn with_listen_url(mut self, url: impl Into<String>) -> Self {
        self.listen_url = Some(url.into());
        self
    }

    /// Record `created_at` and `updated_at` from `clock` instead of [`SystemClock`].
    ///
    /// The offset is `signal_log.id`. Nothing in this bus orders or expires rows
    /// by these timestamps, so the clock is an audit source, not a deadline.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// How long `watch` waits for a notification before reading the table again.
    ///
    /// The default is 50 ms. A test that wants to observe a lost notification
    /// shortens this; production keeps the default. Correctness does not depend
    /// on the notification arriving.
    #[must_use]
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// The interval `watch` uses when a notification does not arrive.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// Apply embedded migrations. Idempotent: an already-migrated database is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`BusError::Backend`] when a migration statement fails.
    pub async fn migrate(&self) -> Result<(), BusError> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    /// The pool, for a test that shares this database with another connection.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl Wakeup for PgWakeup {
    async fn notify(&self, instance: InstanceId, hint: Hint) -> Result<Offset, BusError> {
        let id = parse_id(&instance)?;
        let (code, hint_ref) = encode_hint(&hint)?;
        let now = recorded_at(self.clock.now())?;
        // Insert and notify in one transaction so a committed row is visible
        // to a poll that races the notification, and a rolled-back insert
        // sends nothing.
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let log_id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO signal_log (created_at, updated_at, instance_id, hint, hint_ref) \
             VALUES ($1, $1, $2, $3, $4) \
             RETURNING id",
        )
        .bind(now)
        .bind(id)
        .bind(code)
        .bind(hint_ref)
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?;
        sqlx::query("SELECT pg_notify($1, '')")
            .bind(CHANNEL)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        offset_from_id(log_id)
    }

    async fn watch(&self, from: Offset) -> Result<SignalStream, BusError> {
        // Reject a cursor that would skip a deleted row before the stream starts.
        // A live watcher hits the same check after each wait.
        ensure_retained(&self.pool, from).await?;
        let pool = self.pool.clone();
        let poll_interval = self.poll_interval;
        let url = self.listen_url.clone().ok_or_else(|| {
            BusError::Backend(
                "watch needs a listen URL; construct with connect or with_listen_url".into(),
            )
        })?;
        let mut listener = PgListener::connect(&url).await.map_err(backend)?;
        listener.ignore_pool_close_event(true);
        listener.eager_reconnect(true);
        listener.listen(CHANNEL).await.map_err(backend)?;
        Ok(Box::pin(stream::unfold(
            Some(Watch {
                pool,
                listener,
                from,
                poll_interval,
            }),
            |watch| async move {
                let mut watch = watch?;
                loop {
                    match next_signal(&watch.pool, watch.from).await {
                        Ok(Some(signal)) => {
                            watch.from = signal.offset;
                            return Some((Ok(signal), Some(watch)));
                        }
                        Err(error) => return Some((Err(error), None)),
                        Ok(None) => {}
                    }
                    // A notification is a hint to look again. The timeout is
                    // the backstop when that hint is lost. Either way the next
                    // iteration reads the table.
                    let notified = tokio::time::timeout(watch.poll_interval, watch.listener.recv());
                    if let Ok(Err(error)) = notified.await {
                        return Some((Err(backend(error)), None));
                    }
                }
            },
        )))
    }

    async fn commit_offset(&self, offset: Offset) -> Result<(), BusError> {
        let stored = i64::try_from(offset.0).map_err(|_| {
            BusError::Backend(format!("offset {} does not fit in a bigint", offset.0))
        })?;
        let now = recorded_at(self.clock.now())?;
        // GREATEST keeps a late or repeated commit from moving the cursor backwards.
        sqlx::query(
            "INSERT INTO signal_cursor \
                 (created_at, updated_at, consumer_group, committed_offset) \
             VALUES ($1, $1, $2, $3) \
             ON CONFLICT (consumer_group) DO UPDATE \
             SET committed_offset = GREATEST(signal_cursor.committed_offset, EXCLUDED.committed_offset), \
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(now)
        .bind(&self.consumer_group)
        .bind(stored)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn committed(&self) -> Result<Offset, BusError> {
        let stored = sqlx::query_scalar::<_, i64>(
            "SELECT committed_offset FROM signal_cursor WHERE consumer_group = $1",
        )
        .bind(&self.consumer_group)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        match stored {
            Some(value) => offset_from_id(value),
            None => Ok(Offset::default()),
        }
    }
}

struct Watch {
    pool: PgPool,
    listener: PgListener,
    from: Offset,
    poll_interval: Duration,
}

async fn next_signal(pool: &PgPool, from: Offset) -> Result<Option<Signal>, BusError> {
    ensure_retained(pool, from).await?;
    let after = i64::try_from(from.0)
        .map_err(|_| BusError::Backend(format!("offset {} does not fit in a bigint", from.0)))?;
    let row = sqlx::query_as::<_, SignalRow>(
        "SELECT id, instance_id, hint, hint_ref \
         FROM signal_log \
         WHERE id > $1 \
         ORDER BY id \
         LIMIT 1",
    )
    .bind(after)
    .fetch_optional(pool)
    .await
    .map_err(backend)?;
    row.map(SignalRow::try_into).transpose()
}

async fn ensure_retained(pool: &PgPool, from: Offset) -> Result<(), BusError> {
    // MIN of an empty table is SQL NULL. `query_scalar` maps that to None.
    let oldest: Option<i64> = sqlx::query_scalar("SELECT MIN(id) FROM signal_log")
        .fetch_one(pool)
        .await
        .map_err(backend)?;
    // An empty log retains every cursor, including one that points at the
    // last id of a log that has since been fully deleted: there is no row
    // left to miss. A partial delete expires only a cursor behind the oldest
    // remaining id.
    let Some(oldest) = oldest else {
        return Ok(());
    };
    let oldest = u64::try_from(oldest)
        .map_err(|_| BusError::Backend(format!("stored signal id {oldest} is negative")))?;
    if from.0 + 1 < oldest {
        return Err(BusError::OffsetExpired(from));
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct SignalRow {
    id: i64,
    instance_id: Uuid,
    hint: i16,
    hint_ref: Option<String>,
}

impl TryFrom<SignalRow> for Signal {
    type Error = BusError;

    fn try_from(row: SignalRow) -> Result<Self, Self::Error> {
        Ok(Self {
            instance: InstanceId::from_string(row.instance_id.to_string()),
            hint: decode_hint(row.hint, row.hint_ref)?,
            offset: offset_from_id(row.id)?,
        })
    }
}

fn encode_hint(hint: &Hint) -> Result<(i16, Option<String>), BusError> {
    Ok(match hint {
        Hint::Timer => (HINT_TIMER, None),
        Hint::ChildDone(id) => (HINT_CHILD_DONE, Some(id.as_str().to_owned())),
        Hint::Event(event_id) => (HINT_EVENT, Some(event_id.clone())),
        Hint::Other => (HINT_OTHER, None),
    })
}

fn decode_hint(code: i16, hint_ref: Option<String>) -> Result<Hint, BusError> {
    match (code, hint_ref) {
        (HINT_TIMER, None) => Ok(Hint::Timer),
        (HINT_CHILD_DONE, Some(id)) => Ok(Hint::ChildDone(InstanceId::from_string(id))),
        (HINT_EVENT, Some(event_id)) => Ok(Hint::Event(event_id)),
        (HINT_OTHER, None) => Ok(Hint::Other),
        _ => Err(BusError::Backend(format!(
            "signal hint {code} does not match its reference"
        ))),
    }
}

fn parse_id(id: &InstanceId) -> Result<Uuid, BusError> {
    Uuid::from_str(id.as_str()).map_err(|error| {
        BusError::Backend(format!(
            "instance id {} is not a UUID: {error}",
            id.as_str()
        ))
    })
}

fn offset_from_id(id: i64) -> Result<Offset, BusError> {
    let value = u64::try_from(id)
        .map_err(|_| BusError::Backend(format!("stored signal id {id} is negative")))?;
    Ok(Offset(value))
}

/// Convert a caller-supplied instant into the row's audit timestamp.
/// No query compares it, so it is not a deadline.
fn recorded_at(value: Timestamp) -> Result<OffsetDateTime, BusError> {
    let millis = i128::from(value.millis());
    OffsetDateTime::from_unix_timestamp_nanos(millis * 1_000_000).map_err(|error| {
        BusError::Backend(format!(
            "timestamp {} is out of range: {error}",
            value.millis()
        ))
    })
}

fn backend(error: impl ToString) -> BusError {
    BusError::Backend(error.to_string())
}
