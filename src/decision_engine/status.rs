//! Read existing process-local observation counters without starting an engine.
use super::{TelemetryRecord, observe::ObserveCounters};
use crate::config::Config;
use parking_lot::Mutex;
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::{LazyLock, Weak, atomic::Ordering},
};
use tokio::sync::mpsc::WeakSender;

struct Entry {
    workspace: PathBuf,
    sender: WeakSender<TelemetryRecord>,
    counters: Weak<ObserveCounters>,
}
static QUEUES: LazyLock<Mutex<Vec<Entry>>> = LazyLock::new(Default::default);

pub(super) fn register(workspace: &Path, sender: WeakSender<TelemetryRecord>, counters: Weak<ObserveCounters>) {
    let mut queues = QUEUES.lock();
    queues.retain(|entry| entry.counters.strong_count() > 0);
    queues.push(Entry {
        workspace: workspace.to_owned(),
        sender,
        counters,
    });
}

#[derive(Debug, Serialize)]
pub(crate) struct DecisionEngineStatus {
    pub enabled: bool,
    pub mode: &'static str,
    pub backend_kind: &'static str,
    pub telemetry_cap: u32,
    pub queue_depth: usize,
    pub dropped: u64,
    pub store_errors: u64,
}

/// The telemetry cap the engine actually applies: `pre` mode records no
/// outcomes, so its effective cap is 0 whatever `telemetry_cap` says.
pub(crate) fn effective_telemetry_cap(engine: &super::config::DecisionEngineConfig) -> u32 {
    if engine.mode == super::config::DecisionMode::Pre {
        0
    } else {
        engine.telemetry_cap
    }
}

pub(crate) fn snapshot(config: &Config) -> DecisionEngineStatus {
    use super::config::DecisionMode;
    let engine = &config.decision_engine;
    let mut row = DecisionEngineStatus {
        enabled: engine.enabled,
        mode: if !engine.enabled {
            "off"
        } else {
            match engine.mode {
                DecisionMode::Pre => "pre",
                DecisionMode::Post => "post",
                DecisionMode::Both => "both",
            }
        },
        backend_kind: super::config::effective_engine_backend(config).kind.as_str(),
        telemetry_cap: effective_telemetry_cap(engine),
        queue_depth: 0,
        dropped: 0,
        store_errors: 0,
    };
    let mut queues = QUEUES.lock();
    queues.retain(|entry| entry.counters.strong_count() > 0);
    for entry in queues.iter().filter(|entry| entry.workspace == config.workspace_dir) {
        if let Some(counters) = entry.counters.upgrade() {
            row.dropped = row.dropped.saturating_add(counters.dropped.load(Ordering::Relaxed));
            row.store_errors = row
                .store_errors
                .saturating_add(counters.store_errors.load(Ordering::Relaxed));
        }
        if let Some(sender) = entry.sender.upgrade() {
            row.queue_depth = row
                .queue_depth
                .saturating_add(sender.max_capacity() - sender.capacity());
        }
    }
    row
}
