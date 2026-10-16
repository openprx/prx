//! Memory draft lifecycle.

use crate::memory::traits::{MemoryDraft, MemoryDraftInput, MemoryPrincipal};
use chrono::Utc;
use rusqlite::params;
use uuid::Uuid;

use super::SqliteMemory;

impl SqliteMemory {
    pub(super) async fn create_memory_draft_impl(&self, input: MemoryDraftInput) -> anyhow::Result<MemoryDraft> {
        let pool = self.pool.clone();

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<MemoryDraft> {
            let mut conn = pool.write();
            let tx = conn.transaction()?;
            let now = Utc::now().to_rfc3339();
            let draft_id = input.draft_id.unwrap_or_else(|| Uuid::new_v4().to_string());
            let category = Self::category_to_str(&input.category);
            let visibility = input.visibility.as_str().to_string();

            tx.execute(
                "INSERT OR IGNORE INTO memory_drafts (
                    draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                    agent_id, persona_id, key, content, category, source_event_id,
                    visibility, status, payload_json, created_at, updated_at
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'pending', ?14, ?15, ?15)",
                params![
                    draft_id,
                    input.workspace_id,
                    input.owner_id,
                    input.worker_run_id,
                    input.parent_run_id,
                    input.session_key,
                    input.agent_id,
                    input.persona_id,
                    input.key,
                    input.content,
                    category,
                    input.source_event_id,
                    visibility,
                    input.payload_json,
                    now,
                ],
            )?;

            let draft = tx.query_row(
                "SELECT id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                        agent_id, persona_id, key, content, category, source_event_id,
                        visibility, status, payload_json, created_at, updated_at
                   FROM memory_drafts
                  WHERE draft_id = ?1
                  LIMIT 1",
                params![draft_id],
                Self::memory_draft_from_row,
            )?;

            tx.execute(
                // C03: the draft's own owner is recorded in the column, not only
                // inside `payload_json`, so a `session`/`private` row born here
                // is attributed and stays readable by its owner-isolated owner.
                "INSERT INTO memory_events (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, agent_id, persona_id, visibility, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, 'memory.draft.created', 'memory_drafts', ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    Uuid::new_v4().to_string(),
                    draft.workspace_id,
                    draft.owner_id,
                    draft.draft_id,
                    draft.session_key,
                    draft.agent_id,
                    draft.persona_id,
                    draft.visibility.as_str(),
                    serde_json::json!({
                        "worker_run_id": draft.worker_run_id,
                        "owner_id": draft.owner_id,
                        "parent_run_id": draft.parent_run_id,
                        "key": draft.key
                    })
                    .to_string(),
                    Utc::now().to_rfc3339(),
                ],
            )?;

            tx.commit()?;
            Ok(draft)
        })
        .await?
    }

    pub(super) async fn list_memory_drafts_for_run_impl(
        &self,
        principal: &MemoryPrincipal,
        worker_run_id: &str,
    ) -> anyhow::Result<Vec<MemoryDraft>> {
        let pool = self.pool.clone();
        let worker_run_id = worker_run_id.to_string();
        // Owner ACL: a caller may only see drafts it owns, plus drafts with no
        // owner (legacy / system-created). System principals bypass the filter.
        let owner = if Self::is_system_principal(principal) {
            None
        } else {
            principal
                .owner_id
                .as_deref()
                .filter(|owner| !owner.trim().is_empty())
                .map(str::to_string)
        };

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MemoryDraft>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            if let Some(owner) = owner {
                let mut stmt = conn.prepare(
                    "SELECT id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                            agent_id, persona_id, key, content, category, source_event_id,
                            visibility, status, payload_json, created_at, updated_at
                       FROM memory_drafts
                      WHERE worker_run_id = ?1 AND (owner_id = ?2 OR owner_id IS NULL)
                      ORDER BY id ASC",
                )?;
                let rows = stmt.query_map(params![worker_run_id, owner], Self::memory_draft_from_row)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
            } else {
                let mut stmt = conn.prepare(
                    "SELECT id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                            agent_id, persona_id, key, content, category, source_event_id,
                            visibility, status, payload_json, created_at, updated_at
                       FROM memory_drafts
                      WHERE worker_run_id = ?1
                      ORDER BY id ASC",
                )?;
                let rows = stmt.query_map(params![worker_run_id], Self::memory_draft_from_row)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
            }
        })
        .await?
    }

    pub(super) async fn merge_memory_draft_impl(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        let pool = self.pool.clone();
        let draft_id = draft_id.to_string();
        let owner = if Self::is_system_principal(principal) {
            None
        } else {
            principal
                .owner_id
                .as_deref()
                .filter(|owner| !owner.trim().is_empty())
                .map(str::to_string)
        };

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<MemoryDraft>> {
            let mut conn = pool.write();
            let tx = conn.transaction()?;
            let mut draft = match tx.query_row(
                "SELECT id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                        agent_id, persona_id, key, content, category, source_event_id,
                        visibility, status, payload_json, created_at, updated_at
                   FROM memory_drafts
                  WHERE draft_id = ?1
                  LIMIT 1",
                params![draft_id],
                Self::memory_draft_from_row,
            ) {
                Ok(draft) => draft,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(error) => return Err(error.into()),
            };

            // Owner ACL: reject merge attempts on drafts owned by a different principal.
            if let Some(owner) = owner.as_deref() {
                if let Some(draft_owner) = draft.owner_id.as_deref() {
                    if draft_owner != owner {
                        anyhow::bail!("memory draft {} is owned by a different principal", draft.draft_id);
                    }
                }
            }

            if draft.status != "pending" && draft.status != "merge_requested" {
                return Ok(Some(draft));
            }

            let now = Utc::now().to_rfc3339();
            let category = Self::category_to_str(&draft.category);
            let memory_id = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO memories (
                    id, key, content, category, created_at, updated_at, session_id,
                    workspace_id, owner_id, agent_id, persona_id, source_event_id, source, visibility
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'memory_draft', ?12)
                 ON CONFLICT(key) DO UPDATE SET
                    content = excluded.content,
                    category = excluded.category,
                    updated_at = excluded.updated_at,
                    session_id = excluded.session_id,
                    workspace_id = excluded.workspace_id,
                    owner_id = excluded.owner_id,
                    agent_id = excluded.agent_id,
                    persona_id = excluded.persona_id,
                    source_event_id = excluded.source_event_id,
                    source = excluded.source,
                    visibility = excluded.visibility",
                params![
                    memory_id,
                    draft.key,
                    draft.content,
                    category,
                    now,
                    draft.session_key,
                    draft.workspace_id,
                    draft.owner_id,
                    draft.agent_id,
                    draft.persona_id,
                    draft.source_event_id,
                    draft.visibility.as_str(),
                ],
            )?;

            tx.execute(
                "UPDATE memory_drafts SET status = 'merged', updated_at = ?2 WHERE draft_id = ?1",
                params![draft.draft_id, now],
            )?;
            draft.status = "merged".to_string();
            draft.updated_at = now;

            for (event_type, subject_table, subject_id) in [
                ("memory.draft.merged", "memory_drafts", draft.draft_id.as_str()),
                ("memory.stored", "memories", draft.key.as_str()),
            ] {
                tx.execute(
                    // C03: see `create_memory_draft` - the owner goes in the column.
                    "INSERT INTO memory_events (
                        event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                        session_key, agent_id, persona_id, visibility, payload_json, created_at
                     )
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        Uuid::new_v4().to_string(),
                        draft.workspace_id,
                        draft.owner_id,
                        event_type,
                        subject_table,
                        subject_id,
                        draft.session_key,
                        draft.agent_id,
                        draft.persona_id,
                        draft.visibility.as_str(),
                        serde_json::json!({
                            "draft_id": draft.draft_id,
                            "owner_id": draft.owner_id,
                            "worker_run_id": draft.worker_run_id,
                            "parent_run_id": draft.parent_run_id,
                            "key": draft.key
                        })
                        .to_string(),
                        Utc::now().to_rfc3339(),
                    ],
                )?;
            }

            tx.commit()?;
            Ok(Some(draft))
        })
        .await?
    }

    pub(super) async fn reject_memory_draft_impl(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<Option<MemoryDraft>> {
        let pool = self.pool.clone();
        let draft_id = draft_id.to_string();
        let reason = reason.map(str::to_string);
        let owner = if Self::is_system_principal(principal) {
            None
        } else {
            principal
                .owner_id
                .as_deref()
                .filter(|owner| !owner.trim().is_empty())
                .map(str::to_string)
        };

        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<MemoryDraft>> {
            let mut conn = pool.write();
            let tx = conn.transaction()?;
            let mut draft = match tx.query_row(
                "SELECT id, draft_id, workspace_id, owner_id, worker_run_id, parent_run_id, session_key,
                        agent_id, persona_id, key, content, category, source_event_id,
                        visibility, status, payload_json, created_at, updated_at
                   FROM memory_drafts
                  WHERE draft_id = ?1
                  LIMIT 1",
                params![draft_id],
                Self::memory_draft_from_row,
            ) {
                Ok(draft) => draft,
                Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
                Err(error) => return Err(error.into()),
            };

            // Owner ACL: reject attempts on drafts owned by a different principal.
            if let Some(owner) = owner.as_deref() {
                if let Some(draft_owner) = draft.owner_id.as_deref() {
                    if draft_owner != owner {
                        anyhow::bail!("memory draft {} is owned by a different principal", draft.draft_id);
                    }
                }
            }

            if draft.status == "merged" || draft.status == "rejected" {
                return Ok(Some(draft));
            }

            let now = Utc::now().to_rfc3339();
            tx.execute(
                "UPDATE memory_drafts SET status = 'rejected', updated_at = ?2 WHERE draft_id = ?1",
                params![draft.draft_id, now],
            )?;
            draft.status = "rejected".to_string();
            draft.updated_at = now;

            tx.execute(
                // C03: see `create_memory_draft` - the owner goes in the column.
                "INSERT INTO memory_events (
                    event_id, workspace_id, owner_id, event_type, subject_table, subject_id,
                    session_key, agent_id, persona_id, visibility, payload_json, created_at
                 )
                 VALUES (?1, ?2, ?3, 'memory.draft.rejected', 'memory_drafts', ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    Uuid::new_v4().to_string(),
                    draft.workspace_id,
                    draft.owner_id,
                    draft.draft_id,
                    draft.session_key,
                    draft.agent_id,
                    draft.persona_id,
                    draft.visibility.as_str(),
                    serde_json::json!({
                        "draft_id": draft.draft_id,
                        "owner_id": draft.owner_id,
                        "worker_run_id": draft.worker_run_id,
                        "parent_run_id": draft.parent_run_id,
                        "key": draft.key,
                        "reason": reason
                    })
                    .to_string(),
                    Utc::now().to_rfc3339(),
                ],
            )?;

            tx.commit()?;
            Ok(Some(draft))
        })
        .await?
    }
}
