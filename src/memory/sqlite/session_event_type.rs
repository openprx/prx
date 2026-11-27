//! SQL of `Memory::newest_session_events_of_type` and
//! `Memory::session_events_of_type_before`: the newest rows of one event type in
//! one session, optionally below a row id. The workspace, session, owner, visibility and agent
//! partition filters are those of `session_context_page` (the read behind
//! `load_session_context_before`), with the same placeholder layout; the role
//! filter is left out, and `event_type` is added.

use super::SQLITE_DIALECT;
use super::SqliteMemory;
use super::events::agent_partition_clause;
use crate::memory::traits::{MemoryPrincipal, MessageEvent};

impl SqliteMemory {
    pub(super) async fn newest_session_events_of_type_impl(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        event_type: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.session_events_of_type_before_impl(principal, session_key, event_type, i64::MAX, limit)
            .await
    }

    /// Rows of `event_type` with `id < before_id`, newest first.
    pub(super) async fn session_events_of_type_before_impl(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        event_type: &str,
        before_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let pool = self.pool.clone();
        let principal = principal.clone();
        let session_key = session_key.to_string();
        let event_type = event_type.to_string();
        let limit = Self::sanitize_conversation_limit(limit);
        // The row-id cursor of `session_context_page`, kept at its neutral value
        // so the placeholders keep the same numbering.
        let after_id: i64 = 0;
        let system_allowed = Self::is_system_principal(&principal);
        let legacy_session_keys = Self::legacy_session_key_params(&principal);
        let mut session_indices = vec![3usize];
        for offset in 0..legacy_session_keys.len() {
            session_indices.push(9 + offset);
        }
        let session_fragment =
            crate::memory::session_predicate::session_key_match_fragment(SQLITE_DIALECT, &session_indices);
        let owner_index = Self::owner_index(9, legacy_session_keys.len());
        let owner_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            owner_index,
            principal.require_owner_match,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        let owner_required_fragment = crate::memory::owner_predicate::owner_match_suffix(
            SQLITE_DIALECT,
            owner_index,
            true,
            crate::memory::owner_predicate::has_owner_id(&principal),
        );
        let event_index = owner_index + usize::from(owner_required_fragment.bound_owner);
        let shared_tiers_arm = crate::memory::owner_predicate::session_context_shared_tiers(&owner_fragment);
        let (partition_sql, partition_values) = agent_partition_clause(&principal, event_index + 1);
        let before_index = event_index + 1 + partition_values.len();
        let sql = format!(
            "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                    parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                    sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                    goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                    content, content_hash, raw_payload_json, visibility, created_at, updated_at
             FROM message_events
             WHERE id > ?1
               AND workspace_id = ?2
               AND {session_fragment}
               AND event_type = ?{event_index}
               AND id < ?{before_index}
               AND (
                   {shared_tiers_arm}
                   OR (visibility = 'agent' AND (
                       (?4 IS NOT NULL AND agent_id = ?4)
                       OR (?5 IS NOT NULL AND persona_id = ?5)
                   ))
                   OR (visibility = 'session'{owner_fragment})
                   OR (visibility = 'private' AND (
                       (?4 IS NOT NULL AND agent_id = ?4)
                       OR (?5 IS NOT NULL AND persona_id = ?5)
                       OR (?6 IS NOT NULL AND sender = ?6)
                   ){owner_required_fragment})
                   OR (visibility = 'system' AND ?7)
               ){partition_sql}
             ORDER BY id DESC
             LIMIT ?8",
            session_fragment = session_fragment.sql,
            owner_fragment = owner_fragment.sql,
            owner_required_fragment = owner_required_fragment.sql,
        );
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&sql)?;
            // Bound in placeholder order: the fixed ones, the legacy keys, the
            // owner, the event type, then the partition values.
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![
                &after_id,
                &principal.workspace_id,
                &session_key,
                &principal.agent_id,
                &principal.persona_id,
                &principal.sender,
                &system_allowed,
                &limit,
            ];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            if owner_required_fragment.bound_owner {
                bind.push(&principal.owner_id);
            }
            bind.push(&event_type);
            for value in &partition_values {
                bind.push(value);
            }
            bind.push(&before_id);
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?
    }
}
