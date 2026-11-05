//! Non-blocking bounded admission with one cancellable telemetry writer.
use super::contract::{ObserveAdmission, TurnOutcome};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The writer must insert then evict oldest records to `cap` atomically.
pub trait TelemetryWriter: Send + 'static {
    fn append_fifo(&mut self, outcome: TurnOutcome, cap: usize) -> anyhow::Result<()>;
}
#[derive(Default)]
pub struct ObserveCounters {
    pub dropped: AtomicU64,
    pub store_errors: AtomicU64,
}
#[derive(Debug, thiserror::Error)]
pub enum ObserveStartError {
    #[error("invalid observation bounds")]
    Bounds,
    #[error("observation worker requires a Tokio runtime")]
    Runtime(#[from] tokio::runtime::TryCurrentError),
    #[error("observation store could not be opened")]
    Store(#[source] anyhow::Error),
    #[error("observation worker could not be started")]
    Worker(#[from] std::io::Error),
}

pub struct ObserveQueue {
    sender: Option<mpsc::Sender<TurnOutcome>>,
    pub counters: Arc<ObserveCounters>,
}
impl ObserveQueue {
    pub fn disabled() -> Self {
        Self {
            sender: None,
            counters: Arc::default(),
        }
    }
    /// A zero cap never calls the store factory or starts a worker.
    pub fn start<F>(
        queue: usize,
        cap: usize,
        cancel: CancellationToken,
        open: F,
    ) -> Result<(Self, Option<tokio::sync::oneshot::Receiver<()>>), ObserveStartError>
    where
        F: FnOnce() -> anyhow::Result<Box<dyn TelemetryWriter>>,
    {
        if queue == 0 || queue > 1_024 || cap > 100_000 {
            return Err(ObserveStartError::Bounds);
        }
        if cap == 0 {
            return Ok((Self::disabled(), None));
        }
        let runtime = tokio::runtime::Handle::try_current()?;
        let mut writer = open().map_err(ObserveStartError::Store)?;
        let (sender, mut receiver) = mpsc::channel(queue);
        let counters = Arc::new(ObserveCounters::default());
        let worker_counters = Arc::clone(&counters);
        let (finished, worker) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("decision-telemetry".into())
            .spawn(move || {
                loop {
                    let outcome = runtime.block_on(async {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {
                                // Stop admission, then persist every accepted outcome.
                                receiver.close();
                                None
                            }
                            outcome = receiver.recv() => outcome,
                        }
                    });
                    let Some(outcome) = outcome else {
                        break;
                    };
                    if writer.append_fifo(outcome, cap).is_err() {
                        worker_counters.store_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("decision engine telemetry write failed");
                    }
                }
                while let Some(outcome) = receiver.blocking_recv() {
                    if writer.append_fifo(outcome, cap).is_err() {
                        worker_counters.store_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("decision engine telemetry write failed");
                    }
                }
                let _ = finished.send(());
            })?;
        Ok((
            Self {
                sender: Some(sender),
                counters,
            },
            Some(worker),
        ))
    }
    pub fn observe(&self, outcome: TurnOutcome) -> ObserveAdmission {
        let Some(sender) = &self.sender else {
            return ObserveAdmission::Disabled;
        };
        match sender.try_send(outcome) {
            Ok(()) => ObserveAdmission::Accepted,
            Err(_) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                ObserveAdmission::Dropped
            }
        }
    }
}

/// Dedicated observation store; only the worker owns the connection.
pub struct SqliteTelemetryWriter {
    connection: rusqlite::Connection,
}
impl SqliteTelemetryWriter {
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        #[cfg(test)]
        if let Some(workspace) = path.parent() {
            super::access_spy::record(workspace, false);
        }
        let connection = rusqlite::Connection::open(path)?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS engine_outcomes (sequence INTEGER PRIMARY KEY, decision_id TEXT, tools TEXT NOT NULL, succeeded INTEGER NOT NULL)")?;
        Ok(Self { connection })
    }
    pub fn outcomes(&self) -> anyhow::Result<Vec<TurnOutcome>> {
        let mut statement = self
            .connection
            .prepare("SELECT decision_id, tools, succeeded FROM engine_outcomes ORDER BY sequence")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (id, tools, succeeded) = row?;
            Ok(TurnOutcome::new(id, serde_json::from_str(&tools)?, succeeded)?)
        })
        .collect()
    }
}
impl TelemetryWriter for SqliteTelemetryWriter {
    fn append_fifo(&mut self, outcome: TurnOutcome, cap: usize) -> anyhow::Result<()> {
        let transaction: rusqlite::Transaction<'_> = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO engine_outcomes (decision_id, tools, succeeded) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                outcome.decision_id(),
                serde_json::to_string(outcome.tools())?,
                outcome.succeeded()
            ],
        )?;
        transaction.execute("DELETE FROM engine_outcomes WHERE sequence NOT IN (SELECT sequence FROM engine_outcomes ORDER BY sequence DESC LIMIT ?1)", [i64::try_from(cap)?])?;
        transaction.commit()?;
        Ok(())
    }
}
