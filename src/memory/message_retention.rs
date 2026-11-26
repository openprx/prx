//! Host side of message-log retention: when the store's retention pass runs.
//!
//! The pass itself lives in the SQLite store (`SyncStore::trim_message_log`).
//! This module decides the effective window, computes the cutoff and calls the
//! pass from the two triggers: memory creation (every process start) and an
//! hourly task in the gateway. Nothing here is on a turn path.

use crate::config::SharedConfig;
use crate::memory::Memory;
use crate::memory::sync::{MessageTrim, SyncStore, TrimMode, TrimSkipped};
use chrono::{DateTime, Utc};
use std::sync::{Arc, Once};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The rf sync journal keeps 14 days; the effective minimum is one day more.
const RF_SYNC_WINDOW_DAYS: u32 = 15;
/// Batches per hourly tick in the gateway.
const HOURLY_BATCHES: u32 = 20;
const FIRST_HOURLY_TICK: Duration = Duration::from_secs(60);
const HOURLY: Duration = Duration::from_secs(3600);

/// Maps the configured `memory.message_retention_days` to the effective window.
/// `0` disables the pass. `1..=14` run as 15 and warn once per process. No
/// value is refused.
pub(crate) fn effective_days(days: u32) -> Option<u32> {
    match days {
        0 => None,
        1..RF_SYNC_WINDOW_DAYS => {
            static WARNED: Once = Once::new();
            WARNED.call_once(|| {
                tracing::warn!(
                    days,
                    "memory.message_retention_days is below the rf sync window; using {RF_SYNC_WINDOW_DAYS}"
                );
            });
            Some(RF_SYNC_WINDOW_DAYS)
        }
        _ => Some(days),
    }
}

/// The oldest kept instant for an effective window. `None` only when the
/// window reaches past the representable calendar, where nothing is old enough.
fn cutoff_for(days: u32) -> Option<DateTime<Utc>> {
    Utc::now().checked_sub_days(chrono::Days::new(u64::from(days)))
}

/// Runs one retention pass for the memory backend built at process start.
/// Best effort: failures are logged and swallowed, like the hygiene pass.
pub fn run_once(memory: &dyn Memory, days: u32, max_batches: u32) {
    let Some(days) = effective_days(days) else {
        return;
    };
    let Some(store) = memory.sync_store() else {
        tracing::debug!("message log retention has no effect on this memory backend");
        return;
    };
    let Some(cutoff) = cutoff_for(days) else {
        return;
    };
    match store.trim_message_log(cutoff, max_batches, TrimMode::Startup) {
        Ok(trim) => log_pass(cutoff, &trim),
        Err(error) => tracing::warn!(
            error = %crate::agent::diagnostics::error_class(&error),
            "message log retention pass failed"
        ),
    }
}

/// Runs the pass every hour in the gateway. The window is read from the live
/// configuration on each tick, so an operator who changes the key at runtime
/// gets the next tick applied. Stops when the shutdown token is cancelled.
pub fn spawn_hourly(store: Arc<dyn SyncStore>, config: SharedConfig, shutdown: CancellationToken) {
    tokio::spawn(async move {
        let first = tokio::time::Instant::now() + FIRST_HOURLY_TICK;
        let mut tick = tokio::time::interval_at(first, HOURLY);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }
            let configured = config.pin().effective.memory.message_retention_days;
            let Some(days) = effective_days(configured) else {
                continue;
            };
            let Some(cutoff) = cutoff_for(days) else {
                continue;
            };
            let sync = Arc::clone(&store);
            let pass = crate::runtime::blocking::spawn_blocking(move || {
                sync.trim_message_log(cutoff, HOURLY_BATCHES, TrimMode::Hourly)
            });
            match pass.await {
                Ok(Ok(trim)) => log_pass(cutoff, &trim),
                Ok(Err(error)) => tracing::warn!(
                    error = %crate::agent::diagnostics::error_class(&error),
                    "message log retention pass failed"
                ),
                Err(_) => tracing::warn!("message log retention task did not finish"),
            }
        }
    });
}

/// Logs counts and the effective cutoff only. Never session keys, ids or text.
fn log_pass(cutoff: DateTime<Utc>, trim: &MessageTrim) {
    match trim.skipped {
        TrimSkipped::No => {}
        TrimSkipped::Throttled => {
            tracing::debug!(reason = "throttled", "message log retention pass skipped");
            return;
        }
        TrimSkipped::SnapshotOpen => {
            tracing::debug!(reason = "snapshot_open", "message log retention pass skipped");
            return;
        }
    }
    let deleted = trim.events
        + trim.runs
        + trim.turns
        + trim.attempts
        + trim.links
        + trim.documents
        + trim.traces
        + trim.compactions
        + trim.notices;
    if deleted > 0 {
        tracing::info!(
            cutoff = %cutoff.to_rfc3339(),
            events = trim.events,
            runs = trim.runs,
            turns = trim.turns,
            attempts = trim.attempts,
            links = trim.links,
            documents = trim.documents,
            traces = trim.traces,
            compactions = trim.compactions,
            notices = trim.notices,
            batches = trim.batches,
            "message log retention pass"
        );
    }
}
