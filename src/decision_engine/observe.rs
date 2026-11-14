//! Non-blocking bounded admission with one cancellable telemetry writer.
use super::contract::{AnalysisRecord, ObserveAdmission, TelemetryRecord, TurnOutcome, TurnResultClass};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The writer must insert then evict oldest records of the same kind to
/// `cap` atomically.
pub trait TelemetryWriter: Send + 'static {
    fn append_fifo(&mut self, record: TelemetryRecord, cap: usize) -> anyhow::Result<()>;
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
    sender: Option<mpsc::Sender<TelemetryRecord>>,
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
                    let record = runtime.block_on(async {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {
                                // Stop admission, then persist every accepted outcome.
                                receiver.close();
                                None
                            }
                            record = receiver.recv() => record,
                        }
                    });
                    let Some(record) = record else {
                        break;
                    };
                    if writer.append_fifo(record, cap).is_err() {
                        worker_counters.store_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("decision engine telemetry write failed");
                    }
                }
                while let Some(record) = receiver.blocking_recv() {
                    if writer.append_fifo(record, cap).is_err() {
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
    pub(crate) fn register_status(&self, workspace: &std::path::Path) {
        if let Some(sender) = &self.sender {
            super::status::register(workspace, sender.downgrade(), Arc::downgrade(&self.counters));
        }
    }
    pub fn observe(&self, outcome: TurnOutcome) -> ObserveAdmission {
        self.record(TelemetryRecord::Outcome(outcome))
    }
    /// Admit one record without waiting; a full queue drops and counts it.
    pub fn record(&self, record: TelemetryRecord) -> ObserveAdmission {
        let Some(sender) = &self.sender else {
            return ObserveAdmission::Disabled;
        };
        match sender.try_send(record) {
            Ok(()) => ObserveAdmission::Accepted,
            Err(_) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                ObserveAdmission::Dropped
            }
        }
    }
}

/// The telemetry schema version. Any other `user_version` is recreated
/// fresh: telemetry carries no settlement or authority, so nothing migrates.
const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE engine_analyses(sequence INTEGER PRIMARY KEY,
  decision_id TEXT NOT NULL CHECK(length(decision_id) BETWEEN 1 AND 128),
  created_at_ms INTEGER NOT NULL,
  brief_redacted TEXT NOT NULL CHECK(length(CAST(brief_redacted AS BLOB)) <= 16384),
  brief_sha256 TEXT NOT NULL CHECK(length(brief_sha256) = 64),
  values_json TEXT NOT NULL CHECK(length(CAST(values_json AS BLOB)) <= 16384),
  ma_scope_id TEXT);
CREATE TABLE engine_outcomes(sequence INTEGER PRIMARY KEY,
  decision_id TEXT CHECK(decision_id IS NULL OR length(decision_id) BETWEEN 1 AND 128),
  tools TEXT NOT NULL CHECK(length(CAST(tools AS BLOB)) <= 9000),
  result TEXT NOT NULL CHECK(result IN ('completed','failed','cancelled','refused')),
  cost_micro_usd INTEGER CHECK(cost_micro_usd IS NULL OR cost_micro_usd >= 0),
  duration_ms INTEGER NOT NULL CHECK(duration_ms >= 0),
  ma_scope_id TEXT);
";

/// The telemetry file under the workspace.
pub const TELEMETRY_FILE: &str = "decision-engine.sqlite";

/// Dedicated observation store; only the worker owns the writing connection.
pub struct SqliteTelemetryWriter {
    connection: rusqlite::Connection,
}
impl SqliteTelemetryWriter {
    pub fn open(path: &std::path::Path) -> anyhow::Result<Self> {
        #[cfg(test)]
        if let Some(workspace) = path.parent() {
            super::access_spy::record(workspace, false);
        }
        let mut connection = rusqlite::Connection::open(path)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            let transaction = connection.transaction()?;
            transaction.execute_batch("DROP TABLE IF EXISTS engine_analyses; DROP TABLE IF EXISTS engine_outcomes;")?;
            transaction.execute_batch(SCHEMA)?;
            transaction.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
            transaction.commit()?;
        }
        Ok(Self { connection })
    }

    /// Open an existing store read-only; `None` when there is no file or it
    /// has another schema version. Never creates the file.
    pub fn open_existing(path: &std::path::Path) -> anyhow::Result<Option<Self>> {
        if std::fs::symlink_metadata(path).is_err() {
            return Ok(None);
        }
        let connection = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != SCHEMA_VERSION {
            return Ok(None);
        }
        Ok(Some(Self { connection }))
    }

    /// The last `limit` analyses, newest first.
    pub fn analyses(&self, limit: usize) -> anyhow::Result<Vec<AnalysisRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT decision_id, created_at_ms, brief_redacted, brief_sha256, values_json \
             FROM engine_analyses ORDER BY sequence DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([i64::try_from(limit)?], |row| {
            Ok(AnalysisRecord::from_row(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        rows.map(|row| Ok(row?)).collect()
    }

    /// The last `limit` outcomes, newest first.
    pub fn outcomes(&self, limit: usize) -> anyhow::Result<Vec<TurnOutcome>> {
        let mut statement = self.connection.prepare(
            "SELECT decision_id, tools, result, cost_micro_usd, duration_ms \
             FROM engine_outcomes ORDER BY sequence DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([i64::try_from(limit)?], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (id, tools, result, cost, duration) = row?;
            let result = TurnResultClass::parse(&result)
                .ok_or_else(|| anyhow::anyhow!("stored outcome has an unknown result class"))?;
            let cost = cost.map(u64::try_from).transpose()?;
            Ok(TurnOutcome::new(
                id,
                serde_json::from_str(&tools)?,
                result,
                cost,
                u64::try_from(duration)?,
            )?)
        })
        .collect()
    }
}
impl TelemetryWriter for SqliteTelemetryWriter {
    fn append_fifo(&mut self, record: TelemetryRecord, cap: usize) -> anyhow::Result<()> {
        let cap = i64::try_from(cap)?;
        let transaction: rusqlite::Transaction<'_> = self.connection.transaction()?;
        match record {
            TelemetryRecord::Analysis(analysis) => {
                transaction.execute(
                    "INSERT INTO engine_analyses (decision_id, created_at_ms, brief_redacted, brief_sha256, values_json) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        analysis.decision_id(),
                        analysis.created_at_ms(),
                        analysis.brief_redacted(),
                        analysis.brief_sha256(),
                        analysis.values_json()
                    ],
                )?;
                transaction.execute(
                    "DELETE FROM engine_analyses WHERE sequence NOT IN \
                     (SELECT sequence FROM engine_analyses ORDER BY sequence DESC LIMIT ?1)",
                    [cap],
                )?;
            }
            TelemetryRecord::Outcome(outcome) => {
                let cost = outcome.cost_micro_usd().map(i64::try_from).transpose()?;
                transaction.execute(
                    "INSERT INTO engine_outcomes (decision_id, tools, result, cost_micro_usd, duration_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        outcome.decision_id(),
                        serde_json::to_string(outcome.tools())?,
                        outcome.result().as_str(),
                        cost,
                        i64::try_from(outcome.duration_ms())?
                    ],
                )?;
                transaction.execute(
                    "DELETE FROM engine_outcomes WHERE sequence NOT IN \
                     (SELECT sequence FROM engine_outcomes ORDER BY sequence DESC LIMIT ?1)",
                    [cap],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
}
