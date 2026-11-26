//! SQL of `Memory::newest_owner_events_of_type`: the newest rows of one event
//! type across every session of one owner. The owner, workspace, erasure and
//! inactive-incarnation filters and the partition are those of
//! `search_transcripts_impl`; the visibility arm of the session read is left
//! out, because the owner and partition already bound the rows.

use super::SqliteMemory;
use super::events::{MA_SCOPE_COLUMN, numbered_placeholders, partition_conjunct};
use crate::memory::traits::{MemoryPrincipal, MessageEvent};
use crate::memory::{AgentPartition, PartitionKey};

impl SqliteMemory {
    pub(super) async fn newest_owner_events_of_type_impl(
        &self,
        principal: &MemoryPrincipal,
        event_type: &str,
        after_id: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        // Owner and partition rules are those of `search_transcripts_impl`: no
        // owner reads nothing; no partition is the Main partition; a partition
        // of another owner or workspace reads nothing.
        let Some(owner) = principal.owner_id.clone() else {
            return Ok(Vec::new());
        };
        let workspace = principal.workspace_id.clone();
        let partition = match principal.agent_partition.clone() {
            Some(partition) => partition,
            None => AgentPartition {
                workspace_id: workspace.clone(),
                owner_id: owner.clone(),
                key: PartitionKey::Main,
            },
        };
        if partition.owner_id != owner || partition.workspace_id != workspace {
            return Ok(Vec::new());
        }
        let pool = self.pool.clone();
        let event_type = event_type.to_string();
        let after_id = after_id.unwrap_or(0);
        let limit = Self::sanitize_conversation_limit(limit);
        // Placeholders: ?1 workspace, ?2 owner, ?3 event type, ?4 row-id cursor,
        // then the partition values, then the limit.
        let (partition_sql, partition_values) =
            partition_conjunct(MA_SCOPE_COLUMN, Some(&partition), numbered_placeholders(5));
        let limit_marker = 5 + partition_values.len();
        let sql = format!(
            "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                    parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                    sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                    goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                    content, content_hash, raw_payload_json, visibility, created_at, updated_at
             FROM message_events
             WHERE workspace_id = ?1
               AND owner_id = ?2
               AND event_type = ?3
               AND id > ?4
               AND NOT EXISTS (SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = message_events.ma_scope_id)
               AND (message_events.ma_scope_id IS NULL OR EXISTS (
                    SELECT 1 FROM agent_scopes s
                    JOIN agent_incarnations i
                      ON i.workspace_id = s.workspace_id AND i.owner_id = s.owner_id
                     AND i.agent_key = s.agent_key AND i.incarnation = s.incarnation
                    WHERE s.scope_id = message_events.ma_scope_id AND i.state = 'active')){partition_sql}
             ORDER BY id DESC
             LIMIT ?{limit_marker}"
        );
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&sql)?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![&workspace, &owner, &event_type, &after_id];
            for value in &partition_values {
                bind.push(value);
            }
            bind.push(&limit);
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?
    }
}
