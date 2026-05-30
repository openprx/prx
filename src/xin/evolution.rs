//! Deterministic xin entrypoint for evolution proposal drafting.

use crate::config::Config;
use crate::self_system::evolution::config::{EvolutionConfig, EvolutionMode};
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use uuid::Uuid;

const DRAFT_RUNTIME: &str = "xin:scheduler";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DraftEvolutionTickReport {
    pub mode: EvolutionMode,
    pub drafted: usize,
    pub judged: usize,
    pub applied: usize,
}

pub struct DraftEvolutionScheduler {
    config: Config,
    evolution_config: EvolutionConfig,
}

impl DraftEvolutionScheduler {
    pub const fn new(config: Config, evolution_config: EvolutionConfig) -> Self {
        Self {
            config,
            evolution_config,
        }
    }

    pub fn load(config: Config) -> Result<Self> {
        let evolution_config = load_evolution_config_sync(&config)?;
        Ok(Self::new(config, evolution_config))
    }

    pub fn tick(&self) -> Result<DraftEvolutionTickReport> {
        let mode = self.evolution_config.runtime.mode.clone();
        let db_path = self.config.workspace_dir.join("memory").join("brain.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to prepare memory dir: {}", parent.display()))?;
        }

        let conn =
            Connection::open(&db_path).with_context(|| format!("failed to open memory db: {}", db_path.display()))?;
        ensure_evolution_schema(&conn)?;

        let source_counts = SourceCounts::load(&conn).unwrap_or_default();
        let candidate_hash = source_counts.candidate_hash();
        let target_resource = serde_json::json!({
            "kind": "semantic_memory",
            "memory_id": "xin:memory_evolution",
            "scope": "workspace"
        });
        let proposed_change = serde_json::json!({
            "kind": "memory_update",
            "new_value": {
                "summary": "scheduled memory evolution review",
                "message_events": source_counts.message_events,
                "memory_events": source_counts.memory_events,
                "memories": source_counts.memories
            },
            "diff_hash": candidate_hash
        });

        let existing: Option<String> = conn
            .query_row(
                "SELECT draft_id FROM evolution_proposals
                 WHERE workspace_id = ?1
                   AND created_by_runtime = ?2
                   AND evidence_hashes_json = ?3
                   AND applied_at IS NULL
                 ORDER BY id DESC
                 LIMIT 1",
                params![
                    self.workspace_id(),
                    DRAFT_RUNTIME,
                    serde_json::to_string(&vec![candidate_hash.clone()])?
                ],
                |row| row.get(0),
            )
            .optional()?;

        if existing.is_none() {
            let draft_id = format!("evo-{}", Uuid::now_v7());
            let now = Utc::now().to_rfc3339();
            let evidence_hashes_json = serde_json::to_string(&vec![candidate_hash])?;
            conn.execute(
                "INSERT INTO evolution_proposals (
                    draft_id, owner_id, principal_id, workspace_id, topic_id, task_id,
                    source_message_event_ids_json, source_memory_event_ids_json, evidence_hashes_json,
                    target_resource_json, proposed_change_json, risk_level, mode,
                    created_at, created_by_runtime
                 )
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, '[]', '[]', ?6, ?7, ?8, 'low', ?9, ?10, ?11)",
                params![
                    draft_id,
                    "self_system",
                    "xin:scheduler",
                    self.workspace_id(),
                    "xin:memory_evolution",
                    evidence_hashes_json,
                    target_resource.to_string(),
                    proposed_change.to_string(),
                    mode_to_db(&mode),
                    now,
                    DRAFT_RUNTIME,
                ],
            )?;
            conn.execute(
                "INSERT INTO evolution_proposal_events (
                    draft_id, event_type, occurred_at, actor, payload_json
                 )
                 VALUES (?1, 'proposal.drafted', ?2, ?3, ?4)",
                params![
                    draft_id,
                    now,
                    DRAFT_RUNTIME,
                    serde_json::json!({ "mode": mode_to_db(&mode), "source": "xin:memory_evolution" }).to_string(),
                ],
            )?;
        }

        Ok(DraftEvolutionTickReport {
            mode,
            drafted: usize::from(existing.is_none()),
            judged: 0,
            applied: 0,
        })
    }

    fn workspace_id(&self) -> String {
        self.config.workspace_dir.to_string_lossy().to_string()
    }
}

#[derive(Debug, Clone, Default)]
struct SourceCounts {
    message_events: i64,
    memory_events: i64,
    memories: i64,
}

impl SourceCounts {
    fn load(conn: &Connection) -> Result<Self> {
        Ok(Self {
            message_events: count_table(conn, "message_events")?,
            memory_events: count_table(conn, "memory_events")?,
            memories: count_table(conn, "memories")?,
        })
    }

    fn candidate_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.message_events.to_be_bytes());
        hasher.update(self.memory_events.to_be_bytes());
        hasher.update(self.memories.to_be_bytes());
        hex::encode(hasher.finalize())
    }
}

fn count_table(conn: &Connection, table: &str) -> Result<i64> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        params![table],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(0);
    }
    let sql = format!("SELECT COUNT(*) FROM {table}");
    Ok(conn.query_row(&sql, [], |row| row.get(0))?)
}

pub fn ensure_evolution_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS evolution_proposals (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            draft_id TEXT NOT NULL UNIQUE,
            owner_id TEXT NOT NULL,
            principal_id TEXT NOT NULL,
            workspace_id TEXT NOT NULL,
            topic_id TEXT,
            task_id TEXT,
            source_message_event_ids_json TEXT NOT NULL DEFAULT '[]',
            source_memory_event_ids_json TEXT NOT NULL DEFAULT '[]',
            evidence_hashes_json TEXT NOT NULL DEFAULT '[]',
            target_resource_json TEXT NOT NULL,
            proposed_change_json TEXT NOT NULL,
            risk_level TEXT NOT NULL CHECK (risk_level IN ('low','medium','high','critical')),
            mode TEXT NOT NULL CHECK (mode IN ('draft_only','shadow','auto')),
            created_at TEXT NOT NULL,
            created_by_runtime TEXT NOT NULL,
            judge_verdict_json TEXT,
            applied_at TEXT,
            applied_by TEXT,
            rollback_anchor_json TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_evolution_proposals_owner_workspace
            ON evolution_proposals(owner_id, workspace_id, created_at DESC);
        CREATE INDEX IF NOT EXISTS idx_evolution_proposals_pending
            ON evolution_proposals(workspace_id, applied_at) WHERE applied_at IS NULL;
        CREATE INDEX IF NOT EXISTS idx_evolution_proposals_task
            ON evolution_proposals(task_id) WHERE task_id IS NOT NULL;

        CREATE TABLE IF NOT EXISTS evolution_proposal_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            draft_id TEXT NOT NULL,
            event_type TEXT NOT NULL CHECK (event_type IN (
                'proposal.drafted','proposal.judged','proposal.approved',
                'proposal.rejected','proposal.applied','proposal.rollback','proposal.evidence_mismatch'
            )),
            occurred_at TEXT NOT NULL,
            actor TEXT NOT NULL,
            payload_json TEXT,
            FOREIGN KEY (draft_id) REFERENCES evolution_proposals(draft_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_evolution_proposal_events_draft
            ON evolution_proposal_events(draft_id, occurred_at);",
    )?;
    Ok(())
}

fn load_evolution_config_sync(config: &Config) -> Result<EvolutionConfig> {
    let path = discover_evolution_config_path(config);
    if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read evolution config: {}", path.display()))?;
        toml::from_str::<EvolutionConfig>(&raw)
            .with_context(|| format!("failed to parse evolution config: {}", path.display()))
    } else {
        Ok(EvolutionConfig::default())
    }
}

fn discover_evolution_config_path(config: &Config) -> PathBuf {
    if let Some(raw) = config.self_system.evolution_config_path.as_deref() {
        let path = PathBuf::from(raw);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }

    let candidates = [
        config.workspace_dir.join("evolution_config.toml"),
        PathBuf::from("evolution_config.toml"),
        PathBuf::from("config/evolution_config.toml"),
    ];
    candidates
        .iter()
        .find(|path| path.exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone())
}

const fn mode_to_db(mode: &EvolutionMode) -> &'static str {
    match mode {
        EvolutionMode::DraftOnly => "draft_only",
        EvolutionMode::Shadow => "shadow",
        EvolutionMode::Auto => "auto",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let mut config = Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        config.self_system.evolution_enabled = true;
        config
    }

    #[test]
    fn draft_evolution_scheduler_creates_draft_without_agent_run() {
        let tmp = TempDir::new().unwrap();
        let scheduler = DraftEvolutionScheduler::new(test_config(&tmp), EvolutionConfig::default());

        let report = scheduler.tick().unwrap();

        assert_eq!(report.mode, EvolutionMode::DraftOnly);
        assert_eq!(report.drafted, 1);
        assert_eq!(report.applied, 0);

        let conn = Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
        let proposal_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM evolution_proposals", [], |row| row.get(0))
            .unwrap();
        let event_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM evolution_proposal_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(proposal_count, 1);
        assert_eq!(event_count, 1);
    }

    #[test]
    fn draft_evolution_scheduler_is_idempotent_for_same_source_counts() {
        let tmp = TempDir::new().unwrap();
        let scheduler = DraftEvolutionScheduler::new(test_config(&tmp), EvolutionConfig::default());

        assert_eq!(scheduler.tick().unwrap().drafted, 1);
        assert_eq!(scheduler.tick().unwrap().drafted, 0);
    }
}
