//! SQL of `Memory::turn_final_rows`: the `turn.finalized` rows of a set of
//! runs of one owner, in one indexed query.

use super::SqliteMemory;
use super::events::numbered_placeholders;
use crate::memory::traits::MessageEvent;

/// Run ids read in one query at most; the rest are left out.
const TURN_FINAL_ROWS_MAX: usize = 50;

impl SqliteMemory {
    pub(super) async fn turn_final_rows_impl(
        &self,
        workspace_id: &str,
        owner_id: &str,
        run_ids: &[String],
    ) -> anyhow::Result<Vec<MessageEvent>> {
        anyhow::ensure!(
            !workspace_id.is_empty() && !owner_id.is_empty(),
            "turn final lookup requires workspace and owner"
        );
        let keys: Vec<String> = run_ids
            .iter()
            .take(TURN_FINAL_ROWS_MAX)
            .map(|run_id| format!("turn:{run_id}:final"))
            .collect();
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let pool = self.pool.clone();
        let workspace = workspace_id.to_string();
        let owner = owner_id.to_string();
        // Placeholders: ?1 workspace, ?2 owner, then one per key.
        let mut next = numbered_placeholders(3);
        let marks = keys.iter().map(|_| next()).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
                    parent_session_key, run_id, parent_run_id, agent_id, persona_id,
                    sender, recipient, role, event_type, source_ref_json, subject_ref_json,
                    goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch, config_generation_id, config_source_revision,
                    content, content_hash, raw_payload_json, visibility, created_at, updated_at
             FROM message_events
             WHERE workspace_id = ?1
               AND owner_id = ?2
               AND event_type = 'turn.finalized'
               AND idempotency_key IN ({marks})"
        );
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Vec<MessageEvent>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let mut stmt = conn.prepare(&sql)?;
            let mut bind: Vec<&dyn rusqlite::types::ToSql> = vec![&workspace, &owner];
            for key in &keys {
                bind.push(key);
            }
            let rows = stmt.query_map(bind.as_slice(), Self::message_event_from_row)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?
    }
}
