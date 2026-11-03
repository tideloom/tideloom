// Copyright 2024 Tideloom Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::types::time::OffsetDateTime;
use tideloom_core::Timestamp;
use tideloom_postgres::PgDefinitions;
use tideloom_postgres::PgEventBus;
use tideloom_postgres::PgStore;
use tideloom_postgres::PgWakeup;
use tideloom_runtime::Clock;
use tideloom_runtime::DefinitionSource;
use tideloom_runtime::EmitExecutor;
use tideloom_runtime::Engine;
use tideloom_runtime::EngineConfig;
use tideloom_runtime::EngineParts;
use tideloom_runtime::EventSink;
use tideloom_runtime::EventSource;
use tideloom_runtime::HttpExecutor;
use tideloom_runtime::InstanceStore;
use tideloom_runtime::SystemClock;
use tideloom_runtime::Wakeup;
use tideloom_runtime::WorkerId;
use tokio_util::sync::CancellationToken;

const CONSUMER_GROUP: &str = "workers";
const RETAIN_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let url = std::env::var("DATABASE_URL")?;
    let worker = WorkerId(std::env::var("WORKER_ID")?);
    let store = Arc::new(PgStore::connect(&url).await?);
    store.migrate().await?;
    let wakeup = Arc::new(PgWakeup::connect(&url, CONSUMER_GROUP).await?);
    let events = Arc::new(PgEventBus::connect_waking(&url).await?);
    let definitions = Arc::new(PgDefinitions::connect(&url).await?);
    let clock = Arc::new(SystemClock);
    let engine = Engine::new(EngineParts {
        worker,
        store: Arc::clone(&store) as Arc<dyn InstanceStore>,
        wakeup: Arc::clone(&wakeup) as Arc<dyn Wakeup>,
        definitions: Arc::clone(&definitions) as Arc<dyn DefinitionSource>,
        executors: vec![
            Arc::new(HttpExecutor::new(
                Arc::clone(&definitions) as Arc<dyn DefinitionSource>
            )),
            Arc::new(EmitExecutor::new(Arc::clone(&events) as Arc<dyn EventSink>)),
        ],
        events_out: Arc::clone(&events) as Arc<dyn EventSink>,
        events_in: Arc::clone(&events) as Arc<dyn EventSource>,
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
    });

    let shutdown = CancellationToken::new();
    let retention = tokio::spawn(retain_old_rows(
        store.pool().clone(),
        clock,
        shutdown.clone(),
    ));
    let run = engine.run(EngineConfig::default(), shutdown.clone());
    tokio::pin!(run);
    let result = tokio::select! {
        result = &mut run => result,
        signal = shutdown_signal() => {
            signal?;
            shutdown.cancel();
            run.await
        }
    };
    shutdown.cancel();
    retention
        .await
        .map_err(|error| format!("retention task failed: {error}"))?;
    result?;
    Ok(())
}

async fn retain_old_rows(pool: PgPool, clock: Arc<SystemClock>, shutdown: CancellationToken) {
    let mut ticks = tokio::time::interval(Duration::from_secs(60 * 60));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticks.tick().await;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = ticks.tick() => {
                let millis = clock.now().millis();
                let cutoff = Timestamp::from_millis(
                    millis.saturating_sub(u64::try_from(RETAIN_FOR.as_millis()).unwrap_or(u64::MAX)),
                );
                if let Err(error) = delete_old_rows(&pool, cutoff).await {
                    eprintln!("retention cleanup failed: {error}");
                }
            }
        }
    }
}

async fn delete_old_rows(pool: &PgPool, cutoff: Timestamp) -> Result<(), Box<dyn Error>> {
    let nanos = i128::from(cutoff.millis()) * 1_000_000;
    let cutoff = OffsetDateTime::from_unix_timestamp_nanos(nanos)?;
    sqlx::query("DELETE FROM signal_log WHERE created_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM event_inbox WHERE is_acked = 1 AND updated_at < $1")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() -> std::io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}
