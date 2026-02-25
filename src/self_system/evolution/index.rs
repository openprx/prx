use crate::self_system::evolution::analyzer::AnalyzerDataSource;
use crate::self_system::evolution::record::{DecisionLog, EvolutionLog, MemoryAccessLog};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// Incremental import counters returned by the JSONL to SQLite indexer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImportSummary {
    pub scanned_files: u32,
    pub imported_memory_rows: u32,
    pub imported_decision_rows: u32,
    pub imported_evolution_rows: u32,
}

/// Full-text search hit projected from indexed evolution data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub experiment_id: String,
    pub source: String,
    pub input_context: String,
    pub action_taken: String,
    pub trigger_reason: String,
}

/// SQLite-backed query index built from tiered JSONL evolution logs.
#[derive(Debug, Clone)]
pub struct JsonlToSqliteIndexer {
    sqlite_path: PathBuf,
    storage_root: PathBuf,
}

impl JsonlToSqliteIndexer {
    pub fn new(sqlite_path: impl AsRef<Path>, storage_root: impl AsRef<Path>) -> Result<Self> {
        let this = Self {
            sqlite_path: sqlite_path.as_ref().to_path_buf(),
            storage_root: storage_root.as_ref().to_path_buf(),
        };
        this.init_schema()?;
        Ok(this)
    }

    pub fn sqlite_path(&self) -> &Path {
        &self.sqlite_path
    }

    pub fn import_incremental(&self) -> Result<ImportSummary> {
        let mut conn = Connection::open(&self.sqlite_path)?;
        self.init_schema_on(&conn)?;

        let mut summary = ImportSummary::default();
        for (kind, rel) in [
            (LogKind::Memory, "memory_access"),
            (LogKind::Decision, "decisions"),
            (LogKind::Evolution, "evolution"),
        ] {
            let base = self.storage_root.join(rel);
            for path in list_jsonl_files(&base)? {
                summary.scanned_files = summary.scanned_files.saturating_add(1);
                let imported = self.import_file(&mut conn, kind, &path)?;
                match kind {
                    LogKind::Memory => {
                        summary.imported_memory_rows =
                            summary.imported_memory_rows.saturating_add(imported)
                    }
                    LogKind::Decision => {
                        summary.imported_decision_rows =
                            summary.imported_decision_rows.saturating_add(imported)
                    }
                    LogKind::Evolution => {
                        summary.imported_evolution_rows =
                            summary.imported_evolution_rows.saturating_add(imported)
                    }
                }
            }
        }

        Ok(summary)
    }

    pub fn by_date_range(&self, start_date: &str, end_date: &str) -> Result<Vec<String>> {
        let conn = Connection::open(&self.sqlite_path)?;
        let mut rows = Vec::new();

        for sql in [
            "SELECT raw_json FROM memory_access_index WHERE event_date BETWEEN ?1 AND ?2 ORDER BY timestamp ASC",
            "SELECT raw_json FROM decision_index WHERE event_date BETWEEN ?1 AND ?2 ORDER BY timestamp ASC",
            "SELECT raw_json FROM evolution_index WHERE event_date BETWEEN ?1 AND ?2 ORDER BY timestamp ASC",
        ] {
            let mut stmt = conn.prepare(sql)?;
            let mapped = stmt.query_map(params![start_date, end_date], |row| row.get::<_, String>(0))?;
            for item in mapped {
                rows.push(item?);
            }
        }

        Ok(rows)
    }

    pub fn by_experiment(&self, experiment_id: &str) -> Result<Vec<String>> {
        let conn = Connection::open(&self.sqlite_path)?;
        let mut rows = Vec::new();

        for sql in [
            "SELECT raw_json FROM memory_access_index WHERE experiment_id = ?1 ORDER BY timestamp ASC",
            "SELECT raw_json FROM decision_index WHERE experiment_id = ?1 ORDER BY timestamp ASC",
            "SELECT raw_json FROM evolution_index WHERE experiment_id = ?1 ORDER BY timestamp ASC",
        ] {
            let mut stmt = conn.prepare(sql)?;
            let mapped = stmt.query_map(params![experiment_id], |row| row.get::<_, String>(0))?;
            for item in mapped {
                rows.push(item?);
            }
        }

        Ok(rows)
    }

    pub fn by_layer(&self, layer: &str) -> Result<Vec<EvolutionLog>> {
        let conn = Connection::open(&self.sqlite_path)?;
        let mut stmt = conn.prepare(
            "SELECT raw_json FROM evolution_index WHERE layer = ?1 ORDER BY timestamp ASC",
        )?;
        let mut rows = Vec::new();
        let mut malformed_rows = 0u32;
        let mapped = stmt.query_map(params![layer], |row| row.get::<_, String>(0))?;
        for item in mapped {
            let raw = item?;
            match serde_json::from_str::<EvolutionLog>(&raw) {
                Ok(parsed) => rows.push(parsed),
                Err(_) => malformed_rows = malformed_rows.saturating_add(1),
            };
        }
        if malformed_rows > 0 {
            tracing::warn!(
                layer,
                malformed_rows,
                "skipped malformed evolution rows from sqlite index"
            );
        }
        Ok(rows)
    }

    pub fn full_text_search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let conn = Connection::open(&self.sqlite_path)?;
        let mut stmt = conn.prepare(
            "SELECT experiment_id, source, input_context, action_taken, trigger_reason FROM evolution_fts WHERE evolution_fts MATCH ?1 LIMIT ?2",
        )?;
        let mapped = stmt.query_map(params![query, limit.max(1) as i64], |row| {
            Ok(SearchHit {
                experiment_id: row.get(0)?,
                source: row.get(1)?,
                input_context: row.get(2)?,
                action_taken: row.get(3)?,
                trigger_reason: row.get(4)?,
            })
        })?;

        let mut out = Vec::new();
        for item in mapped {
            out.push(item?);
        }
        Ok(out)
    }

    pub fn update_memory_annotation(
        &self,
        experiment_id: &str,
        trace_id: &str,
        memory_id: &str,
        was_useful: bool,
        confidence: f64,
        needs_human_review: bool,
    ) -> Result<usize> {
        let conn = Connection::open(&self.sqlite_path)?;
        let updated = conn.execute(
            "UPDATE memory_access_index
             SET was_useful = ?1, annotation_confidence = ?2, needs_human_review = ?3
             WHERE experiment_id = ?4 AND trace_id = ?5 AND memory_id = ?6",
            params![
                i64::from(was_useful),
                confidence,
                i64::from(needs_human_review),
                experiment_id,
                trace_id,
                memory_id
            ],
        )?;
        Ok(updated)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = Connection::open(&self.sqlite_path)?;
        self.init_schema_on(&conn)
    }

    fn init_schema_on(&self, conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;

            CREATE TABLE IF NOT EXISTS import_offsets (
                file_path TEXT PRIMARY KEY,
                offset INTEGER NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS memory_access_index (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_date TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                experiment_id TEXT NOT NULL,
                trace_id TEXT NOT NULL,
                memory_id TEXT NOT NULL,
                task_context TEXT NOT NULL,
                was_useful INTEGER,
                annotation_confidence REAL,
                needs_human_review INTEGER NOT NULL DEFAULT 0,
                raw_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS decision_index (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_date TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                experiment_id TEXT NOT NULL,
                trace_id TEXT NOT NULL,
                outcome TEXT NOT NULL,
                input_context TEXT NOT NULL,
                action_taken TEXT NOT NULL,
                raw_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS evolution_index (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_date TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                experiment_id TEXT NOT NULL,
                layer TEXT NOT NULL,
                trigger_reason TEXT NOT NULL,
                result TEXT,
                raw_json TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_memory_date ON memory_access_index(event_date);
            CREATE INDEX IF NOT EXISTS idx_memory_experiment ON memory_access_index(experiment_id);
            CREATE INDEX IF NOT EXISTS idx_decision_date ON decision_index(event_date);
            CREATE INDEX IF NOT EXISTS idx_decision_experiment ON decision_index(experiment_id);
            CREATE INDEX IF NOT EXISTS idx_evolution_date ON evolution_index(event_date);
            CREATE INDEX IF NOT EXISTS idx_evolution_experiment ON evolution_index(experiment_id);
            CREATE INDEX IF NOT EXISTS idx_evolution_layer ON evolution_index(layer);

            CREATE VIRTUAL TABLE IF NOT EXISTS evolution_fts USING fts5(
                experiment_id UNINDEXED,
                source UNINDEXED,
                input_context,
                action_taken,
                trigger_reason
            );
            ",
        )?;
        ensure_import_dedup_schema(conn)?;
        Ok(())
    }

    fn import_file(&self, conn: &mut Connection, kind: LogKind, path: &Path) -> Result<u32> {
        let canonical = path
            .canonicalize()
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_string();
        let mut previous_offset = load_offset(conn, &canonical)?.max(0) as usize;
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read jsonl file: {}", path.display()))?;
        if bytes.len() < previous_offset {
            previous_offset = 0;
        }
        if previous_offset == bytes.len() {
            return Ok(0);
        }

        let slice = &bytes[previous_offset..];
        let Some(last_newline_pos) = slice.iter().rposition(|b| *b == b'\n') else {
            tracing::debug!(
                path = %path.display(),
                "jsonl import deferred because no complete newline-terminated line exists"
            );
            return Ok(0);
        };
        let processed_len = last_newline_pos + 1;
        let commit_offset = previous_offset + processed_len;
        let processed = &slice[..processed_len];
        let mut line_number = count_lines(&bytes[..previous_offset]) + 1;
        let logical_date = parse_logical_date(path);
        let mut imported = 0u32;

        let tx = conn.transaction()?;
        for raw in processed.split(|b| *b == b'\n') {
            if raw.is_empty() {
                continue;
            }
            let line = String::from_utf8_lossy(raw);
            let normalized = line.trim();
            if normalized.is_empty() {
                line_number += 1;
                continue;
            }
            if try_reserve_dedup_key(
                &tx,
                kind.as_key(),
                &logical_date,
                &canonical,
                normalized,
                line_number,
            )? {
                imported = imported.saturating_add(insert_line(&tx, kind, normalized)?);
            }
            line_number += 1;
        }
        tx.execute(
            "INSERT INTO import_offsets(file_path, offset, updated_at)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(file_path) DO UPDATE SET offset=excluded.offset, updated_at=excluded.updated_at",
            params![canonical, commit_offset as i64, Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;

        Ok(imported)
    }
}

#[async_trait]
impl AnalyzerDataSource for JsonlToSqliteIndexer {
    async fn read_decisions_since(&self, since: DateTime<Utc>) -> Result<Vec<DecisionLog>> {
        let conn = Connection::open(&self.sqlite_path)?;
        let mut stmt = conn.prepare(
            "SELECT raw_json FROM decision_index WHERE timestamp >= ?1 ORDER BY timestamp ASC",
        )?;
        let mapped = stmt.query_map(params![since.to_rfc3339()], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        let mut malformed_rows = 0u32;
        for item in mapped {
            let raw = item?;
            match serde_json::from_str::<DecisionLog>(&raw) {
                Ok(parsed) => out.push(parsed),
                Err(_) => malformed_rows = malformed_rows.saturating_add(1),
            };
        }
        if malformed_rows > 0 {
            tracing::warn!(
                malformed_rows,
                "skipped malformed decision rows from sqlite index"
            );
        }
        Ok(out)
    }

    async fn read_memory_access_since(&self, since: DateTime<Utc>) -> Result<Vec<MemoryAccessLog>> {
        let conn = Connection::open(&self.sqlite_path)?;
        let mut stmt = conn.prepare(
            "SELECT raw_json FROM memory_access_index WHERE timestamp >= ?1 ORDER BY timestamp ASC",
        )?;
        let mapped = stmt.query_map(params![since.to_rfc3339()], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        let mut malformed_rows = 0u32;
        for item in mapped {
            let raw = item?;
            match serde_json::from_str::<MemoryAccessLog>(&raw) {
                Ok(parsed) => out.push(parsed),
                Err(_) => malformed_rows = malformed_rows.saturating_add(1),
            };
        }
        if malformed_rows > 0 {
            tracing::warn!(
                malformed_rows,
                "skipped malformed memory rows from sqlite index"
            );
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogKind {
    Memory,
    Decision,
    Evolution,
}

impl LogKind {
    fn as_key(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Decision => "decision",
            Self::Evolution => "evolution",
        }
    }
}

fn insert_line(conn: &Connection, kind: LogKind, raw_line: &str) -> Result<u32> {
    match kind {
        LogKind::Memory => {
            let parsed = match serde_json::from_str::<MemoryAccessLog>(raw_line) {
                Ok(item) => item,
                Err(err) => {
                    tracing::debug!(
                        error = %err,
                        "skipping malformed memory jsonl line during sqlite import"
                    );
                    return Ok(0);
                }
            };
            let event_date = extract_date(&parsed.timestamp);
            conn.execute(
                "INSERT INTO memory_access_index(
                    event_date, timestamp, experiment_id, trace_id, memory_id,
                    task_context, was_useful, annotation_confidence, needs_human_review, raw_json
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, json(?9))",
                params![
                    event_date,
                    parsed.timestamp,
                    parsed.experiment_id,
                    parsed.trace_id,
                    parsed.memory_id,
                    parsed.task_context,
                    parsed.was_useful.map(i64::from),
                    parsed.annotation_confidence,
                    raw_line,
                ],
            )?;
            Ok(1)
        }
        LogKind::Decision => {
            let parsed = match serde_json::from_str::<DecisionLog>(raw_line) {
                Ok(item) => item,
                Err(err) => {
                    tracing::debug!(
                        error = %err,
                        "skipping malformed decision jsonl line during sqlite import"
                    );
                    return Ok(0);
                }
            };
            let event_date = extract_date(&parsed.timestamp);
            conn.execute(
                "INSERT INTO decision_index(
                    event_date, timestamp, experiment_id, trace_id, outcome,
                    input_context, action_taken, raw_json
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, json(?8))",
                params![
                    event_date,
                    parsed.timestamp,
                    parsed.experiment_id,
                    parsed.trace_id,
                    serde_json::to_string(&parsed.outcome)?,
                    parsed.input_context,
                    parsed.action_taken,
                    raw_line,
                ],
            )?;
            conn.execute(
                "INSERT INTO evolution_fts(experiment_id, source, input_context, action_taken, trigger_reason)
                 VALUES(?1, 'decision', ?2, ?3, '')",
                params![parsed.experiment_id, parsed.input_context, parsed.action_taken],
            )?;
            Ok(1)
        }
        LogKind::Evolution => {
            let parsed = match serde_json::from_str::<EvolutionLog>(raw_line) {
                Ok(item) => item,
                Err(err) => {
                    tracing::debug!(
                        error = %err,
                        "skipping malformed evolution jsonl line during sqlite import"
                    );
                    return Ok(0);
                }
            };
            let event_date = extract_date(&parsed.timestamp);
            conn.execute(
                "INSERT INTO evolution_index(
                    event_date, timestamp, experiment_id, layer,
                    trigger_reason, result, raw_json
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, json(?7))",
                params![
                    event_date,
                    parsed.timestamp,
                    parsed.experiment_id,
                    serde_json::to_string(&parsed.layer)?,
                    parsed.trigger_reason,
                    parsed
                        .result
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    raw_line,
                ],
            )?;
            conn.execute(
                "INSERT INTO evolution_fts(experiment_id, source, input_context, action_taken, trigger_reason)
                 VALUES(?1, 'evolution', '', '', ?2)",
                params![parsed.experiment_id, parsed.trigger_reason],
            )?;
            Ok(1)
        }
    }
}

fn load_offset(conn: &Connection, file_path: &str) -> Result<i64> {
    let mut stmt = conn.prepare("SELECT offset FROM import_offsets WHERE file_path = ?1")?;
    let row = stmt.query_row(params![file_path], |row| row.get::<_, i64>(0));
    match row {
        Ok(offset) => Ok(offset),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
        Err(err) => Err(err.into()),
    }
}

fn count_lines(bytes: &[u8]) -> i64 {
    bytes.iter().filter(|b| **b == b'\n').count() as i64
}

fn ensure_import_dedup_schema(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(import_dedup)")?;
    let mut rows = stmt.query([])?;
    let mut columns = Vec::new();
    while let Some(row) = rows.next()? {
        columns.push(row.get::<_, String>(1)?);
    }

    if columns.is_empty() {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS import_dedup (
                kind TEXT NOT NULL,
                logical_date TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                line_number INTEGER NOT NULL,
                PRIMARY KEY(kind, logical_date, content_hash, line_number)
            );
            ",
        )?;
        return Ok(());
    }

    if columns == vec!["content_hash".to_string(), "line_number".to_string()] {
        conn.execute_batch(
            "
            ALTER TABLE import_dedup RENAME TO import_dedup_legacy;
            CREATE TABLE import_dedup (
                kind TEXT NOT NULL,
                logical_date TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                line_number INTEGER NOT NULL,
                PRIMARY KEY(kind, logical_date, content_hash, line_number)
            );
            INSERT OR IGNORE INTO import_dedup(kind, logical_date, content_hash, line_number)
            SELECT 'unknown', 'unknown', content_hash, line_number
            FROM import_dedup_legacy;
            DROP TABLE import_dedup_legacy;
            ",
        )?;
    }

    Ok(())
}

fn try_reserve_dedup_key(
    conn: &Connection,
    kind: &str,
    logical_date: &str,
    file_path: &str,
    line: &str,
    line_number: i64,
) -> Result<bool> {
    let dedup_dimension = if logical_date == "unknown" {
        format!("unknown::{file_path}")
    } else {
        logical_date.to_string()
    };
    let content_hash = hash_line(line);
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO import_dedup(kind, logical_date, content_hash, line_number) VALUES(?1, ?2, ?3, ?4)",
        params![kind, dedup_dimension, content_hash, line_number],
    )?;
    Ok(inserted > 0)
}

fn hash_line(line: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(line.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn extract_date(ts: &str) -> String {
    ts.split('T').next().unwrap_or(ts).to_string()
}

fn parse_logical_date(path: &Path) -> String {
    let Some(stem) = path.file_stem().and_then(|v| v.to_str()) else {
        tracing::debug!(
            path = %path.display(),
            "missing file stem when parsing logical date; using unknown"
        );
        return "unknown".to_string();
    };
    match chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d") {
        Ok(date) => date.to_string(),
        Err(err) => {
            tracing::debug!(
                path = %path.display(),
                error = %err,
                "failed to parse logical date from jsonl file stem; using unknown"
            );
            "unknown".to_string()
        }
    }
}

fn list_jsonl_files(base: &Path) -> Result<Vec<PathBuf>> {
    if !base.exists() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for tier in ["hot", "warm", "cold"] {
        let dir = base.join(tier);
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|v| v.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_system::evolution::record::{
        Actor, ChangeType, DataBasis, DecisionType, EvolutionLayer, MemoryAction, Outcome, TaskType,
    };
    use crate::self_system::evolution::storage::{
        AsyncJsonlWriter, JsonlRetentionPolicy, JsonlStoragePaths,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[tokio::test]
    async fn indexer_imports_and_queries_incrementally() {
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        let writer = Arc::new(
            AsyncJsonlWriter::new(
                JsonlStoragePaths::new(logs.clone()),
                JsonlRetentionPolicy::default(),
                1,
            )
            .await
            .unwrap(),
        );

        writer
            .append_memory_access(&MemoryAccessLog {
                timestamp: "2026-02-24T00:00:00Z".to_string(),
                experiment_id: "exp-1".to_string(),
                trace_id: "trace-1".to_string(),
                action: MemoryAction::Read,
                memory_id: "m1".to_string(),
                task_context: "ctx".to_string(),
                task_type: TaskType::Planning,
                actor: Actor::Agent,
                was_useful: Some(true),
                useful_annotation_source: None,
                annotation_confidence: Some(0.8),
                tokens_consumed: 1,
            })
            .await
            .unwrap();
        writer
            .append_decision(&DecisionLog {
                timestamp: "2026-02-24T00:01:00Z".to_string(),
                experiment_id: "exp-1".to_string(),
                trace_id: "trace-1".to_string(),
                decision_type: DecisionType::ToolSelection,
                task_type: TaskType::Planning,
                risk_level: 1,
                actor: Actor::Agent,
                input_context: "context tokens".to_string(),
                action_taken: "do action".to_string(),
                outcome: Outcome::Success,
                tokens_used: 2,
                latency_ms: 1,
                user_correction: None,
                config_snapshot_hash: "cfg".to_string(),
            })
            .await
            .unwrap();
        writer
            .append_evolution(&EvolutionLog {
                experiment_id: "exp-1".to_string(),
                timestamp: "2026-02-24T00:02:00Z".to_string(),
                layer: EvolutionLayer::Memory,
                change_type: ChangeType::Tune,
                before_value: "a".to_string(),
                after_value: "b".to_string(),
                trigger_reason: "trigger reason".to_string(),
                data_basis: DataBasis {
                    sample_count: 1,
                    time_range_days: 1,
                    key_metrics: HashMap::new(),
                    patterns_found: Vec::new(),
                },
                result: None,
            })
            .await
            .unwrap();
        writer.flush().await.unwrap();

        let indexer = JsonlToSqliteIndexer::new(dir.path().join("idx.db"), &logs).unwrap();
        let first = indexer.import_incremental().unwrap();
        assert_eq!(first.imported_memory_rows, 1);
        assert_eq!(first.imported_decision_rows, 1);
        assert_eq!(first.imported_evolution_rows, 1);

        let second = indexer.import_incremental().unwrap();
        assert_eq!(second.imported_memory_rows, 0);
        assert_eq!(second.imported_decision_rows, 0);
        assert_eq!(second.imported_evolution_rows, 0);

        let by_exp = indexer.by_experiment("exp-1").unwrap();
        assert_eq!(by_exp.len(), 3);

        let hits = indexer.full_text_search("trigger", 10).unwrap();
        assert!(!hits.is_empty());
    }

    #[test]
    fn indexer_dedups_after_tier_migration_by_content_hash_and_line_number() {
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        let hot = logs.join("memory_access/hot/2026-02-24.jsonl");
        let warm = logs.join("memory_access/warm/2026-02-24.jsonl");
        std::fs::create_dir_all(hot.parent().unwrap()).unwrap();
        std::fs::create_dir_all(warm.parent().unwrap()).unwrap();

        let line = r#"{"timestamp":"2026-02-24T00:00:00Z","experiment_id":"exp-1","trace_id":"trace-1","action":"read","memory_id":"m1","task_context":"ctx","task_type":"planning","actor":"agent","was_useful":true,"useful_annotation_source":null,"annotation_confidence":0.8,"tokens_consumed":1}"#;
        std::fs::write(&hot, format!("{line}\n")).unwrap();

        let indexer = JsonlToSqliteIndexer::new(dir.path().join("idx.db"), &logs).unwrap();
        let first = indexer.import_incremental().unwrap();
        assert_eq!(first.imported_memory_rows, 1);

        std::fs::rename(&hot, &warm).unwrap();
        let second = indexer.import_incremental().unwrap();
        assert_eq!(second.imported_memory_rows, 0);
    }

    #[test]
    fn indexer_imports_same_content_same_line_from_different_files() {
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        let first = logs.join("memory_access/hot/2026-02-24.jsonl");
        let second = logs.join("memory_access/hot/2026-02-25.jsonl");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second.parent().unwrap()).unwrap();

        let line = r#"{"timestamp":"2026-02-24T00:00:00Z","experiment_id":"exp-1","trace_id":"trace-1","action":"read","memory_id":"m1","task_context":"ctx","task_type":"planning","actor":"agent","was_useful":true,"useful_annotation_source":null,"annotation_confidence":0.8,"tokens_consumed":1}"#;
        std::fs::write(&first, format!("{line}\n")).unwrap();
        std::fs::write(&second, format!("{line}\n")).unwrap();

        let indexer = JsonlToSqliteIndexer::new(dir.path().join("idx.db"), &logs).unwrap();
        let summary = indexer.import_incremental().unwrap();
        assert_eq!(summary.imported_memory_rows, 2);
    }

    #[test]
    fn indexer_resets_offset_after_file_truncate() {
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        let path = logs.join("memory_access/hot/2026-02-24.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let mk = |exp: &str, mem: &str| {
            format!(
                "{{\"timestamp\":\"2026-02-24T00:00:00Z\",\"experiment_id\":\"{exp}\",\"trace_id\":\"trace-1\",\"action\":\"read\",\"memory_id\":\"{mem}\",\"task_context\":\"ctx\",\"task_type\":\"planning\",\"actor\":\"agent\",\"was_useful\":true,\"useful_annotation_source\":null,\"annotation_confidence\":0.8,\"tokens_consumed\":1}}"
            )
        };
        std::fs::write(
            &path,
            format!("{}\n{}\n", mk("exp-a", "m1"), mk("exp-a", "m2")),
        )
        .unwrap();

        let indexer = JsonlToSqliteIndexer::new(dir.path().join("idx.db"), &logs).unwrap();
        let first = indexer.import_incremental().unwrap();
        assert_eq!(first.imported_memory_rows, 2);

        std::fs::write(&path, format!("{}\n", mk("exp-b", "m3"))).unwrap();
        let second = indexer.import_incremental().unwrap();
        assert_eq!(second.imported_memory_rows, 1);
    }

    #[test]
    fn indexer_only_advances_offset_to_last_complete_line() {
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        let path = logs.join("memory_access/hot/2026-02-24.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let complete = r#"{"timestamp":"2026-02-24T00:00:00Z","experiment_id":"exp-1","trace_id":"trace-1","action":"read","memory_id":"m1","task_context":"ctx","task_type":"planning","actor":"agent","was_useful":true,"useful_annotation_source":null,"annotation_confidence":0.8,"tokens_consumed":1}"#;
        let partial = r#"{"timestamp":"2026-02-24T00:00:00Z","experiment_id":"exp-1","trace_id":"trace-1","action":"read","memory_id":"m2""#;
        std::fs::write(&path, format!("{complete}\n{partial}")).unwrap();

        let indexer = JsonlToSqliteIndexer::new(dir.path().join("idx.db"), &logs).unwrap();
        let first = indexer.import_incremental().unwrap();
        assert_eq!(first.imported_memory_rows, 1);

        std::fs::write(&path, format!("{complete}\n{partial},\"task_context\":\"ctx\",\"task_type\":\"planning\",\"actor\":\"agent\",\"was_useful\":true,\"useful_annotation_source\":null,\"annotation_confidence\":0.8,\"tokens_consumed\":1}}\n")).unwrap();
        let second = indexer.import_incremental().unwrap();
        assert_eq!(second.imported_memory_rows, 1);
    }

    #[test]
    fn indexer_does_not_cross_dedup_unknown_logical_date_between_files() {
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        let first = logs.join("memory_access/hot/non_date_a.jsonl");
        let second = logs.join("memory_access/hot/non_date_b.jsonl");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        let line = r#"{"timestamp":"2026-02-24T00:00:00Z","experiment_id":"exp-1","trace_id":"trace-1","action":"read","memory_id":"m1","task_context":"ctx","task_type":"planning","actor":"agent","was_useful":true,"useful_annotation_source":null,"annotation_confidence":0.8,"tokens_consumed":1}"#;
        std::fs::write(&first, format!("{line}\n")).unwrap();
        std::fs::write(&second, format!("{line}\n")).unwrap();

        let indexer = JsonlToSqliteIndexer::new(dir.path().join("idx.db"), &logs).unwrap();
        let summary = indexer.import_incremental().unwrap();
        assert_eq!(summary.imported_memory_rows, 2);
    }
}
