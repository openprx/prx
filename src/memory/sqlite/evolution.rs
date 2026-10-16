//! Evolution proposal persistence.

use crate::memory::traits::MemoryPrincipal;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};

use super::SqliteMemory;

impl SqliteMemory {
    /// Owner-id constraint for evolution-proposal ACL.
    ///
    /// System principals (`self_system`/`router`/`internal`/`system` as agent or
    /// owner) may query/act globally → `None`. Everyone else is constrained to
    /// their effective owner id (or empty string, which matches no row).
    fn evolution_owner_scope(principal: &MemoryPrincipal) -> Option<String> {
        const SYSTEM_IDS: &[&str] = &["self_system", "router", "internal", "system"];
        let is_system = principal.agent_id.as_deref().is_some_and(|id| SYSTEM_IDS.contains(&id))
            || principal
                .persona_id
                .as_deref()
                .is_some_and(|id| SYSTEM_IDS.contains(&id))
            || principal.owner_id.as_deref().is_some_and(|id| SYSTEM_IDS.contains(&id));
        if is_system {
            return None;
        }
        Some(principal.effective_owner_id().unwrap_or_default())
    }

    pub(super) async fn create_evolution_proposal_impl(
        &self,
        draft: crate::self_system::evolution::EvolutionProposalDraft,
    ) -> anyhow::Result<String> {
        use crate::self_system::evolution::proposal::ProposalRowValues;
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<String> {
            let values = ProposalRowValues::encode(&draft)?;
            let conn = pool.write();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO evolution_proposals (
                    draft_id, owner_id, principal_id, workspace_id, topic_id, task_id,
                    source_message_event_ids_json, source_memory_event_ids_json, evidence_hashes_json,
                    target_resource_json, proposed_change_json, risk_level, mode,
                    created_at, created_by_runtime, judge_verdict_json, applied_at, applied_by, rollback_anchor_json
                 )
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                params![
                    values.draft_id,
                    values.owner_id,
                    values.principal_id,
                    values.workspace_id,
                    values.topic_id,
                    values.task_id,
                    values.source_message_event_ids_json,
                    values.source_memory_event_ids_json,
                    values.evidence_hashes_json,
                    values.target_resource_json,
                    values.proposed_change_json,
                    values.risk_level,
                    values.mode,
                    values.created_at.to_rfc3339(),
                    values.created_by_runtime,
                    values.judge_verdict_json,
                    values.applied_at.map(|ts| ts.to_rfc3339()),
                    values.applied_by,
                    values.rollback_anchor_json,
                ],
            )?;
            conn.execute(
                "INSERT INTO evolution_proposal_events (draft_id, event_type, occurred_at, actor, payload_json)
                 VALUES (?1, 'proposal.drafted', ?2, ?3, ?4)",
                params![
                    values.draft_id,
                    now,
                    values.created_by_runtime,
                    Some(serde_json::json!({ "mode": values.mode, "risk_level": values.risk_level }).to_string()),
                ],
            )?;
            Ok(values.draft_id)
        })
        .await?
    }

    pub(super) async fn list_evolution_proposals_impl(
        &self,
        principal: &MemoryPrincipal,
        filter: crate::self_system::evolution::ProposalFilter,
    ) -> anyhow::Result<Vec<crate::self_system::evolution::EvolutionProposalDraft>> {
        let pool = self.pool.clone();
        let owner_scope = Self::evolution_owner_scope(principal);
        crate::runtime::blocking::spawn_blocking(
            move || -> anyhow::Result<Vec<crate::self_system::evolution::EvolutionProposalDraft>> {
                let reader = pool.read()?;
                let conn = reader.connection()?;
                // Parameterized dynamic WHERE: every predicate binds a value; no
                // user text is interpolated into the SQL string.
                let mut sql = String::from(
                    "SELECT draft_id, owner_id, principal_id, workspace_id, topic_id, task_id,
                            source_message_event_ids_json, source_memory_event_ids_json, evidence_hashes_json,
                            target_resource_json, proposed_change_json, risk_level, mode,
                            created_at, created_by_runtime, judge_verdict_json, applied_at, applied_by,
                            rollback_anchor_json
                       FROM evolution_proposals
                      WHERE 1 = 1",
                );
                let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
                if let Some(owner_id) = owner_scope {
                    sql.push_str(" AND owner_id = ?");
                    binds.push(Box::new(owner_id));
                }
                if let Some(workspace_id) = filter.workspace_id {
                    sql.push_str(" AND workspace_id = ?");
                    binds.push(Box::new(workspace_id));
                }
                if let Some(owner_id) = filter.owner_id {
                    sql.push_str(" AND owner_id = ?");
                    binds.push(Box::new(owner_id));
                }
                if let Some(topic_id) = filter.topic_id {
                    sql.push_str(" AND topic_id = ?");
                    binds.push(Box::new(topic_id));
                }
                if let Some(task_id) = filter.task_id {
                    sql.push_str(" AND task_id = ?");
                    binds.push(Box::new(task_id));
                }
                if let Some(mode) = filter.mode {
                    sql.push_str(" AND mode = ?");
                    binds.push(Box::new(
                        crate::self_system::evolution::proposal::mode_to_db(&mode).to_string(),
                    ));
                }
                if let Some(judged) = filter.judged {
                    if judged {
                        sql.push_str(" AND judge_verdict_json IS NOT NULL");
                    } else {
                        sql.push_str(" AND judge_verdict_json IS NULL");
                    }
                }
                if let Some(applied) = filter.applied {
                    if applied {
                        sql.push_str(" AND applied_at IS NOT NULL");
                    } else {
                        sql.push_str(" AND applied_at IS NULL");
                    }
                }
                if let Some(since) = filter.since {
                    sql.push_str(" AND created_at >= ?");
                    binds.push(Box::new(since.to_rfc3339()));
                }
                sql.push_str(" ORDER BY id DESC");
                if filter.limit > 0 {
                    sql.push_str(" LIMIT ?");
                    binds.push(Box::new(i64::try_from(filter.limit).unwrap_or(i64::MAX)));
                }

                let mut stmt = conn.prepare(&sql)?;
                let bind_refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| &**b as &dyn rusqlite::ToSql).collect();
                let rows = stmt.query_map(bind_refs.as_slice(), Self::evolution_proposal_from_row)?;
                let mut proposals = Vec::new();
                for row in rows {
                    proposals.push(row??);
                }
                Ok(proposals)
            },
        )
        .await?
    }

    pub(super) async fn get_evolution_proposal_impl(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<crate::self_system::evolution::EvolutionProposalDraft>> {
        let pool = self.pool.clone();
        let owner_scope = Self::evolution_owner_scope(principal);
        let draft_id = draft_id.to_string();
        crate::runtime::blocking::spawn_blocking(
            move || -> anyhow::Result<Option<crate::self_system::evolution::EvolutionProposalDraft>> {
                let reader = pool.read()?;
                let conn = reader.connection()?;
                let proposal = conn
                    .query_row(
                        "SELECT draft_id, owner_id, principal_id, workspace_id, topic_id, task_id,
                                source_message_event_ids_json, source_memory_event_ids_json, evidence_hashes_json,
                                target_resource_json, proposed_change_json, risk_level, mode,
                                created_at, created_by_runtime, judge_verdict_json, applied_at, applied_by,
                                rollback_anchor_json
                           FROM evolution_proposals
                          WHERE draft_id = ?1",
                        params![draft_id],
                        Self::evolution_proposal_from_row,
                    )
                    .optional()?;
                let Some(proposal) = proposal else {
                    return Ok(None);
                };
                let proposal = proposal?;
                // Cross-owner access returns NotFound (None) rather than Forbidden
                // to avoid a cross-owner existence side channel.
                if let Some(owner_id) = owner_scope {
                    if proposal.owner_id != owner_id {
                        return Ok(None);
                    }
                }
                Ok(Some(proposal))
            },
        )
        .await?
    }

    pub(super) async fn update_evolution_proposal_status_impl(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        update: crate::self_system::evolution::ProposalStatusUpdate,
    ) -> anyhow::Result<()> {
        use crate::self_system::evolution::ProposalStatusUpdate;
        let pool = self.pool.clone();
        let owner_scope = Self::evolution_owner_scope(principal);
        let draft_id = draft_id.to_string();
        let actor = principal
            .owner_id
            .clone()
            .or_else(|| principal.agent_id.clone())
            .unwrap_or_else(|| "self_system".to_string());
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            // Fetch the row first to enforce owner ACL and re-judge guard.
            let existing: Option<(String, Option<String>, Option<String>)> = conn
                .query_row(
                    "SELECT owner_id, judge_verdict_json, applied_at FROM evolution_proposals WHERE draft_id = ?1",
                    params![draft_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((owner_id, judge_verdict_json, applied_at)) = existing else {
                anyhow::bail!("evolution proposal {draft_id} not found");
            };
            if let Some(scope_owner) = owner_scope {
                if owner_id != scope_owner {
                    anyhow::bail!("evolution proposal {draft_id} not found");
                }
            }
            let now = Utc::now().to_rfc3339();
            match update {
                ProposalStatusUpdate::Judged { verdict } => {
                    if judge_verdict_json.is_some() {
                        anyhow::bail!("evolution proposal {draft_id} already judged; refusing re-judge");
                    }
                    let verdict_json = serde_json::to_string(&verdict)?;
                    conn.execute(
                        "UPDATE evolution_proposals SET judge_verdict_json = ?1 WHERE draft_id = ?2",
                        params![verdict_json, draft_id],
                    )?;
                    conn.execute(
                        "INSERT INTO evolution_proposal_events (draft_id, event_type, occurred_at, actor, payload_json)
                         VALUES (?1, 'proposal.judged', ?2, ?3, ?4)",
                        params![draft_id, now, actor, Some(verdict_json)],
                    )?;
                }
                ProposalStatusUpdate::Applied {
                    applied_by,
                    rollback_anchor,
                } => {
                    let anchor_json = serde_json::to_string(&rollback_anchor)?;
                    conn.execute(
                        "UPDATE evolution_proposals
                            SET applied_at = ?1, applied_by = ?2, rollback_anchor_json = ?3
                          WHERE draft_id = ?4",
                        params![now, applied_by, anchor_json, draft_id],
                    )?;
                    conn.execute(
                        "INSERT INTO evolution_proposal_events (draft_id, event_type, occurred_at, actor, payload_json)
                         VALUES (?1, 'proposal.applied', ?2, ?3, ?4)",
                        params![draft_id, now, actor, Some(anchor_json)],
                    )?;
                }
                ProposalStatusUpdate::RolledBack => {
                    if applied_at.is_none() {
                        anyhow::bail!("evolution proposal {draft_id} is not applied; cannot roll back");
                    }
                    conn.execute(
                        "UPDATE evolution_proposals SET applied_at = NULL WHERE draft_id = ?1",
                        params![draft_id],
                    )?;
                    conn.execute(
                        "INSERT INTO evolution_proposal_events (draft_id, event_type, occurred_at, actor, payload_json)
                         VALUES (?1, 'proposal.rollback', ?2, ?3, NULL)",
                        params![draft_id, now, actor],
                    )?;
                }
            }
            Ok(())
        })
        .await?
    }

    pub(super) async fn append_evolution_proposal_event_impl(
        &self,
        principal: &MemoryPrincipal,
        draft_id: &str,
        event_type: &str,
        actor: &str,
        payload_json: Option<&str>,
    ) -> anyhow::Result<()> {
        let pool = self.pool.clone();
        let owner_scope = Self::evolution_owner_scope(principal);
        let draft_id = draft_id.to_string();
        let event_type = event_type.to_string();
        let actor = actor.to_string();
        let payload_json = payload_json.map(str::to_string);
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<()> {
            let conn = pool.write();
            let owner: Option<String> = conn
                .query_row(
                    "SELECT owner_id FROM evolution_proposals WHERE draft_id = ?1",
                    params![draft_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(owner) = owner else {
                anyhow::bail!("evolution proposal {draft_id} not found");
            };
            if let Some(scope_owner) = owner_scope {
                if owner != scope_owner {
                    anyhow::bail!("evolution proposal {draft_id} not found");
                }
            }
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO evolution_proposal_events (draft_id, event_type, occurred_at, actor, payload_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![draft_id, event_type, now, actor, payload_json],
            )?;
            Ok(())
        })
        .await?
    }
}
