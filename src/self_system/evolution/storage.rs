use crate::self_system::evolution::record::{DecisionLog, EvolutionLog, MemoryAccessLog};
use crate::self_system::evolution::safety_utils::acquire_file_lock;
use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

/// Storage root path layout for evolution JSONL logs.
#[derive(Debug, Clone)]
pub struct JsonlStoragePaths {
    pub root: PathBuf,
}

impl JsonlStoragePaths {
    /// Create path config rooted at `root`.
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

/// Tiered retention policy for log files.
#[derive(Debug, Clone)]
pub struct JsonlRetentionPolicy {
    pub hot_days: u32,
    pub warm_days: u32,
    pub cold_days: u32,
}

impl Default for JsonlRetentionPolicy {
    fn default() -> Self {
        Self {
            hot_days: 30,
            warm_days: 90,
            cold_days: 180,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogKind {
    MemoryAccess,
    Decisions,
    Evolution,
}

impl LogKind {
    const fn as_dir_name(self) -> &'static str {
        match self {
            Self::MemoryAccess => "memory_access",
            Self::Decisions => "decisions",
            Self::Evolution => "evolution",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetentionTier {
    Hot,
    Warm,
    Cold,
}

impl RetentionTier {
    const fn as_dir_name(self) -> &'static str {
        match self {
            Self::Hot => "hot",
            Self::Warm => "warm",
            Self::Cold => "cold",
        }
    }
}

#[derive(Default)]
struct WriterState {
    buffers: HashMap<PathBuf, Vec<String>>,
    buffered_lines: usize,
}

/// Buffered async JSONL writer for evolution logs.
///
/// This writer is append-only and rotates files by event date (`YYYY-MM-DD.jsonl`).
/// Data is grouped into three kinds:
/// - `memory_access`
/// - `decisions`
/// - `evolution`
pub struct AsyncJsonlWriter {
    paths: JsonlStoragePaths,
    retention: JsonlRetentionPolicy,
    batch_size: usize,
    state: Mutex<WriterState>,
    file_locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl AsyncJsonlWriter {
    /// Create a writer and initialize required directories.
    pub async fn new(paths: JsonlStoragePaths, retention: JsonlRetentionPolicy, batch_size: usize) -> Result<Self> {
        let writer = Self {
            paths,
            retention,
            batch_size: batch_size.max(1),
            state: Mutex::new(WriterState::default()),
            file_locks: Mutex::new(HashMap::new()),
        };
        writer.ensure_directories().await?;
        Ok(writer)
    }

    /// Append a memory-access event into JSONL storage.
    pub async fn append_memory_access(&self, log: &MemoryAccessLog) -> Result<()> {
        self.append_log(LogKind::MemoryAccess, &log.timestamp, log).await
    }

    /// Append a decision event into JSONL storage.
    ///
    /// `input_context` is truncated to 500 characters before write.
    pub async fn append_decision(&self, log: &DecisionLog) -> Result<()> {
        let mut normalized = log.clone();
        normalized.normalize_for_storage();
        self.append_log(LogKind::Decisions, &normalized.timestamp, &normalized)
            .await
    }

    /// Append an evolution-change event into JSONL storage.
    pub async fn append_evolution(&self, log: &EvolutionLog) -> Result<()> {
        self.append_log(LogKind::Evolution, &log.timestamp, log).await
    }

    /// Read memory access events written at or after `since`.
    pub async fn read_memory_access_since(&self, since: DateTime<Utc>) -> Result<Vec<MemoryAccessLog>> {
        self.read_logs_since::<MemoryAccessLog, _>(LogKind::MemoryAccess, since, |item| &item.timestamp)
            .await
    }

    /// Read decision events written at or after `since`.
    pub async fn read_decisions_since(&self, since: DateTime<Utc>) -> Result<Vec<DecisionLog>> {
        self.read_logs_since::<DecisionLog, _>(LogKind::Decisions, since, |item| &item.timestamp)
            .await
    }

    /// Read evolution events written at or after `since`.
    pub async fn read_evolution_since(&self, since: DateTime<Utc>) -> Result<Vec<EvolutionLog>> {
        self.read_logs_since::<EvolutionLog, _>(LogKind::Evolution, since, |item| &item.timestamp)
            .await
    }

    /// Flush all buffered lines to disk.
    pub async fn flush(&self) -> Result<()> {
        let pending = {
            let mut state = self.state.lock().await;
            if state.buffered_lines == 0 {
                return Ok(());
            }
            state.buffered_lines = 0;
            std::mem::take(&mut state.buffers)
        };

        for (path, lines) in pending {
            self.write_lines(&path, &lines).await?;
        }
        self.enforce_retention().await
    }

    /// Enforce tiered retention policy:
    /// - `hot_days`: newest files under `hot/`
    /// - `warm_days`: older files under `warm/`
    /// - `cold_days`: older files under `cold/`
    /// - files older than `cold_days` are removed
    pub async fn enforce_retention(&self) -> Result<()> {
        for kind in [LogKind::MemoryAccess, LogKind::Decisions, LogKind::Evolution] {
            self.reconcile_kind_tiers(kind).await?;
        }
        Ok(())
    }

    async fn append_log<T: serde::Serialize>(&self, kind: LogKind, timestamp: &str, log: &T) -> Result<()> {
        let date = extract_event_date(timestamp);
        let path = self.file_path(kind, date);
        let line = serde_json::to_string(log)?;

        let should_flush = {
            let mut state = self.state.lock().await;
            state.buffers.entry(path).or_default().push(format!("{line}\n"));
            state.buffered_lines += 1;
            state.buffered_lines >= self.batch_size
        };

        if should_flush {
            self.flush().await?;
        }
        Ok(())
    }

    async fn ensure_directories(&self) -> Result<()> {
        for kind in [LogKind::MemoryAccess, LogKind::Decisions, LogKind::Evolution] {
            for tier in [RetentionTier::Hot, RetentionTier::Warm, RetentionTier::Cold] {
                fs::create_dir_all(self.tier_dir(kind, tier)).await?;
            }
        }
        Ok(())
    }

    async fn write_lines(&self, path: &Path, lines: &[String]) -> Result<()> {
        let lock = self.file_mutex_for(path).await;
        let _write_guard = lock.lock().await;
        let _file_guard = acquire_file_lock(path).await?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let mut file = fs::OpenOptions::new().create(true).append(true).open(path).await?;

        for line in lines {
            file.write_all(line.as_bytes()).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        Ok(())
    }

    async fn file_mutex_for(&self, path: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.file_locks.lock().await;
        locks
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn read_logs_since<T, F>(&self, kind: LogKind, since: DateTime<Utc>, ts_of: F) -> Result<Vec<T>>
    where
        T: DeserializeOwned,
        F: Fn(&T) -> &str,
    {
        let mut output = Vec::new();
        for tier in [RetentionTier::Hot, RetentionTier::Warm, RetentionTier::Cold] {
            let dir = self.tier_dir(kind, tier);
            if !path_exists(&dir).await? {
                continue;
            }

            let mut entries = fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.extension().and_then(|v| v.to_str()) != Some("jsonl") {
                    continue;
                }
                let raw = fs::read_to_string(&path).await?;
                let mut malformed_lines = 0u32;
                let mut invalid_timestamps = 0u32;
                for line in raw.lines().filter(|line| !line.trim().is_empty()) {
                    let Ok(parsed) = serde_json::from_str::<T>(line) else {
                        malformed_lines = malformed_lines.saturating_add(1);
                        continue;
                    };
                    let Some(ts) = parse_timestamp_utc(ts_of(&parsed)) else {
                        invalid_timestamps = invalid_timestamps.saturating_add(1);
                        continue;
                    };
                    if ts >= since {
                        output.push(parsed);
                    }
                }
                if malformed_lines > 0 || invalid_timestamps > 0 {
                    tracing::warn!(
                        path = %path.display(),
                        malformed_lines,
                        invalid_timestamps,
                        "skipped unreadable jsonl entries while reading logs"
                    );
                }
            }
        }
        Ok(output)
    }

    async fn reconcile_kind_tiers(&self, kind: LogKind) -> Result<()> {
        for tier in [RetentionTier::Hot, RetentionTier::Warm, RetentionTier::Cold] {
            let dir = self.tier_dir(kind, tier);
            if !path_exists(&dir).await? {
                continue;
            }

            let mut entries = fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.extension().and_then(|v| v.to_str()) != Some("jsonl") {
                    continue;
                }

                let Some(date) = parse_date_from_file_path(&path) else {
                    tracing::warn!(
                        path = %path.display(),
                        "skipping retention reconcile for file with invalid date stem"
                    );
                    continue;
                };
                let age_days = age_days(date);
                let target_tier = self.target_tier(age_days);

                match target_tier {
                    Some(expected) if expected == tier => {}
                    Some(expected) => {
                        let target = self.tier_dir(kind, expected).join(
                            path.file_name()
                                .map_or_else(|| "".into(), std::ffi::OsStr::to_os_string),
                        );
                        let (first_path, second_path) = if path <= target {
                            (&path, &target)
                        } else {
                            (&target, &path)
                        };
                        let first_lock = self.file_mutex_for(first_path).await;
                        let second_lock = self.file_mutex_for(second_path).await;
                        let _first_guard = first_lock.lock().await;
                        let _second_guard = second_lock.lock().await;
                        let _source_file_guard = acquire_file_lock(&path).await?;
                        let _target_file_guard = acquire_file_lock(&target).await?;
                        fs::rename(&path, target).await?;
                    }
                    None => {
                        let lock = self.file_mutex_for(&path).await;
                        let _write_guard = lock.lock().await;
                        let _file_guard = acquire_file_lock(&path).await?;
                        fs::remove_file(&path).await?;
                    }
                }
            }
        }
        Ok(())
    }

    const fn target_tier(&self, age_days: u32) -> Option<RetentionTier> {
        if age_days <= self.retention.hot_days {
            return Some(RetentionTier::Hot);
        }
        if age_days <= self.retention.warm_days {
            return Some(RetentionTier::Warm);
        }
        if age_days <= self.retention.cold_days {
            return Some(RetentionTier::Cold);
        }
        None
    }

    fn file_path(&self, kind: LogKind, date: NaiveDate) -> PathBuf {
        let age = age_days(date);
        let tier = self.target_tier(age).unwrap_or(RetentionTier::Cold);
        self.tier_dir(kind, tier)
            .join(format!("{}.jsonl", date.format("%Y-%m-%d")))
    }

    fn tier_dir(&self, kind: LogKind, tier: RetentionTier) -> PathBuf {
        self.paths.root.join(kind.as_dir_name()).join(tier.as_dir_name())
    }
}

async fn path_exists(path: &Path) -> Result<bool> {
    Ok(fs::metadata(path).await.is_ok())
}

fn extract_event_date(timestamp: &str) -> NaiveDate {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|dt| dt.naive_utc().date())
        .unwrap_or_else(|_| Utc::now().date_naive())
}

fn parse_date_from_file_path(path: &Path) -> Option<NaiveDate> {
    let stem = path.file_stem()?.to_str()?;
    match NaiveDate::parse_from_str(stem, "%Y-%m-%d") {
        Ok(date) => Some(date),
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                error = %err,
                "failed to parse date from jsonl file stem"
            );
            None
        }
    }
}

fn age_days(date: NaiveDate) -> u32 {
    let today = Utc::now().date_naive();
    let days = (today - date).num_days();
    if days <= 0 { 0 } else { days as u32 }
}

fn parse_timestamp_utc(raw: &str) -> Option<DateTime<Utc>> {
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(dt) => Some(dt.with_timezone(&Utc)),
        Err(err) => {
            tracing::debug!(
                timestamp = raw,
                error = %err,
                "failed to parse log timestamp"
            );
            None
        }
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::evolution::record::{
        Actor, DataBasis, DecisionType, EvolutionLayer, MemoryAction, Outcome, TaskType,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[tokio::test]
    async fn append_decision_truncates_large_input_context() {
        let dir = tempdir().unwrap();
        let writer = AsyncJsonlWriter::new(
            JsonlStoragePaths::new(dir.path().to_path_buf()),
            JsonlRetentionPolicy::default(),
            1,
        )
        .await
        .unwrap();

        let log = DecisionLog::new(
            "2026-02-24T00:00:00Z".into(),
            "exp".into(),
            "trace".into(),
            DecisionType::ToolSelection,
            TaskType::ToolCall,
            1,
            Actor::Agent,
            "x".repeat(900),
            "action".into(),
            Outcome::Success,
            42,
            20,
            None,
            "hash".into(),
        );
        writer.append_decision(&log).await.unwrap();
        writer.flush().await.unwrap();

        let path = dir.path().join("decisions").join("hot").join("2026-02-24.jsonl");
        let contents = fs::read_to_string(path).await.unwrap();
        let first_line = contents.lines().next().unwrap();
        let stored: DecisionLog = serde_json::from_str(first_line).unwrap();
        assert_eq!(stored.input_context.chars().count(), 500);
    }

    #[tokio::test]
    async fn retention_removes_expired_files() {
        let dir = tempdir().unwrap();
        let writer = AsyncJsonlWriter::new(
            JsonlStoragePaths::new(dir.path().to_path_buf()),
            JsonlRetentionPolicy::default(),
            10,
        )
        .await
        .unwrap();

        let old_date = (Utc::now().date_naive() - chrono::Duration::days(250)).format("%Y-%m-%d");
        let expired_path = dir
            .path()
            .join("memory_access")
            .join("cold")
            .join(format!("{old_date}.jsonl"));
        fs::create_dir_all(expired_path.parent().unwrap()).await.unwrap();
        fs::write(&expired_path, "{}\n").await.unwrap();

        writer.enforce_retention().await.unwrap();
        assert!(!path_exists(&expired_path).await.unwrap());
    }

    #[tokio::test]
    async fn append_supports_all_log_kinds() {
        let dir = tempdir().unwrap();
        let writer = AsyncJsonlWriter::new(
            JsonlStoragePaths::new(dir.path().to_path_buf()),
            JsonlRetentionPolicy::default(),
            3,
        )
        .await
        .unwrap();

        writer
            .append_memory_access(&MemoryAccessLog {
                timestamp: "2026-02-24T00:00:00Z".into(),
                experiment_id: "exp".into(),
                trace_id: "trace".into(),
                action: MemoryAction::Read,
                memory_id: "m1".into(),
                task_context: "ctx".into(),
                task_type: TaskType::Chat,
                actor: Actor::Agent,
                was_useful: None,
                useful_annotation_source: None,
                annotation_confidence: None,
                tokens_consumed: 10,
            })
            .await
            .unwrap();

        writer
            .append_evolution(&EvolutionLog {
                experiment_id: "exp".into(),
                timestamp: "2026-02-24T00:00:00Z".into(),
                layer: EvolutionLayer::Policy,
                change_type: crate::self_system::evolution::record::ChangeType::Update,
                before_value: "{}".into(),
                after_value: "{\"k\":1}".into(),
                trigger_reason: "reason".into(),
                data_basis: DataBasis {
                    sample_count: 1,
                    time_range_days: 1,
                    key_metrics: HashMap::new(),
                    patterns_found: vec![],
                },
                result: None,
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();

        assert!(
            path_exists(&dir.path().join("memory_access").join("hot").join("2026-02-24.jsonl"))
                .await
                .unwrap()
        );
        assert!(
            path_exists(&dir.path().join("evolution").join("hot").join("2026-02-24.jsonl"))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn read_since_filters_older_events() {
        let dir = tempdir().unwrap();
        let writer = AsyncJsonlWriter::new(
            JsonlStoragePaths::new(dir.path().to_path_buf()),
            JsonlRetentionPolicy::default(),
            2,
        )
        .await
        .unwrap();

        writer
            .append_memory_access(&MemoryAccessLog {
                timestamp: "2026-02-23T00:00:00Z".into(),
                experiment_id: "exp-1".into(),
                trace_id: "trace-1".into(),
                action: MemoryAction::Read,
                memory_id: "m-old".into(),
                task_context: "ctx".into(),
                task_type: TaskType::Chat,
                actor: Actor::Agent,
                was_useful: Some(false),
                useful_annotation_source: None,
                annotation_confidence: Some(0.2),
                tokens_consumed: 12,
            })
            .await
            .unwrap();

        writer
            .append_memory_access(&MemoryAccessLog {
                timestamp: "2026-02-24T10:00:00Z".into(),
                experiment_id: "exp-2".into(),
                trace_id: "trace-2".into(),
                action: MemoryAction::Read,
                memory_id: "m-new".into(),
                task_context: "ctx".into(),
                task_type: TaskType::Planning,
                actor: Actor::Agent,
                was_useful: Some(true),
                useful_annotation_source: None,
                annotation_confidence: Some(0.9),
                tokens_consumed: 22,
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let since = chrono::DateTime::parse_from_rfc3339("2026-02-24T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let items = writer.read_memory_access_since(since).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].memory_id, "m-new");
    }

    #[tokio::test]
    async fn concurrent_flush_keeps_jsonl_lines_valid() {
        let dir = tempdir().unwrap();
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(dir.path().to_path_buf()),
                JsonlRetentionPolicy::default(),
                5,
            )
            .await
            .unwrap(),
        );

        let mut tasks = Vec::new();
        for idx in 0..20usize {
            let writer = writer.clone();
            tasks.push(tokio::spawn(async move {
                writer
                    .append_memory_access(&MemoryAccessLog {
                        timestamp: "2026-02-24T12:00:00Z".into(),
                        experiment_id: format!("exp-{idx}"),
                        trace_id: format!("trace-{idx}"),
                        action: MemoryAction::Read,
                        memory_id: format!("m-{idx}"),
                        task_context: "ctx".into(),
                        task_type: TaskType::Planning,
                        actor: Actor::Agent,
                        was_useful: Some(true),
                        useful_annotation_source: None,
                        annotation_confidence: Some(0.8),
                        tokens_consumed: 1,
                    })
                    .await
                    .unwrap();
                writer.flush().await.unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        writer.flush().await.unwrap();

        let path = dir.path().join("memory_access").join("hot").join("2026-02-24.jsonl");
        let raw = fs::read_to_string(path).await.unwrap();
        let lines = raw.lines().filter(|line| !line.trim().is_empty()).collect::<Vec<_>>();
        assert_eq!(lines.len(), 20);
        for line in lines {
            let _: MemoryAccessLog = serde_json::from_str(line).unwrap();
        }
    }

    #[tokio::test]
    async fn retention_waits_for_same_file_mutex_as_writer() {
        let dir = tempdir().unwrap();
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(dir.path().to_path_buf()),
                JsonlRetentionPolicy::default(),
                1,
            )
            .await
            .unwrap(),
        );

        let old_date = (Utc::now().date_naive() - chrono::Duration::days(40)).format("%Y-%m-%d");
        let hot_path = dir
            .path()
            .join("memory_access")
            .join("hot")
            .join(format!("{old_date}.jsonl"));
        fs::create_dir_all(hot_path.parent().unwrap()).await.unwrap();
        fs::write(&hot_path, "{}\n").await.unwrap();

        let lock = writer.file_mutex_for(&hot_path).await;
        let guard = lock.lock().await;
        let writer_for_task = writer.clone();
        let retention_task = tokio::spawn(async move { writer_for_task.enforce_retention().await });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!retention_task.is_finished());
        drop(guard);

        retention_task.await.unwrap().unwrap();
    }
}
