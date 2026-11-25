//! SQL of the session transcript read (`Memory::session_transcript_rows`).
//!
//! The selection is what the Console history shows through the
//! `conversation_turns` projection, read from the log: the user and assistant
//! `message.created` rows of the session, for the owner and agent partition,
//! newest `limit` by event time (row id breaks ties), returned oldest first.
//! A row whose display turn was deleted is left out (a delete tombstones the
//! turn at once and the event row when its run ends), and so is a mailbox
//! continuation input, which only anchors the woken run: the adoption row is
//! the message's one history row.

use super::SqliteMemory;
use super::events::agent_partition_clause;
use super::mailbox_continuation::CONTINUATION_PREDICATE_E;
use crate::memory::traits::{MemoryPrincipal, MessageEvent};

impl SqliteMemory {
    pub(super) async fn session_transcript_rows_impl(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        let pool = self.pool.clone();
        let owner_id = principal
            .owner_id
            .as_deref()
            .filter(|owner| !owner.trim().is_empty())
            .map(str::to_string);
        let session_key = session_key.to_string();
        let limit = Self::sanitize_conversation_limit(limit);
        let legacy_visible = 1_i64;
        // The canonical key binds at `?1`; a distinct legacy key binds after the
        // fixed placeholders, and the partition binds after it.
        let legacy_session_keys: Vec<String> = Self::legacy_session_key_params(principal)
            .into_iter()
            .filter(|key| key.trim() != session_key.trim())
            .collect();
        let mut session_indices = vec![1usize];
        session_indices.extend((0..legacy_session_keys.len()).map(|offset| 5 + offset));
        let session_fragment =
            crate::memory::session_predicate::session_key_match_fragment(super::SQLITE_DIALECT, &session_indices);
        let (partition_sql, partition_values) = agent_partition_clause(principal, 5 + legacy_session_keys.len());
        let sql = format!(
            "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                    parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                    sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                    goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                    content, content_hash, raw_payload_json, visibility, created_at, updated_at
             FROM message_events e
             WHERE {session_fragment}
               AND e.role IN ('user', 'assistant')
               AND e.event_type = 'message.created'
               AND (
                   ?3 = 'system:*'
                   OR e.owner_id = ?3
                   OR (?4 = 1 AND (e.owner_id IS NULL OR e.owner_id = 'legacy:' || e.session_key))
               ){partition_sql}
               AND NOT EXISTS (SELECT 1 FROM conversation_turns t
                               WHERE t.message_event_id = e.event_id AND t.deleted_at IS NOT NULL)
               AND NOT {CONTINUATION_PREDICATE_E}
             ORDER BY julianday(e.created_at) DESC, e.id DESC
             LIMIT ?2",
            session_fragment = session_fragment.sql,
        );
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&sql)?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![&session_key, &limit, &owner_id, &legacy_visible];
            for key in &legacy_session_keys {
                bind.push(key);
            }
            for value in &partition_values {
                bind.push(value);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;
            let mut events = rows.collect::<Result<Vec<_>, _>>()?;
            events.reverse();
            Ok(events)
        })
        .await?
    }
}
