//! Read existing process-local observation counters without starting an engine.
use super::{TurnOutcome, observe::ObserveCounters};
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
    sender: WeakSender<TurnOutcome>,
    counters: Weak<ObserveCounters>,
}
static QUEUES: LazyLock<Mutex<Vec<Entry>>> = LazyLock::new(Default::default);

pub(super) fn register(workspace: &Path, sender: WeakSender<TurnOutcome>, counters: Weak<ObserveCounters>) {
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
        backend_kind: super::config::effective_backend(engine.backend.as_ref(), &config.causal_tree.backend)
            .kind
            .as_str(),
        telemetry_cap: engine.telemetry_cap,
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
