//! Removal of the stored tool-output document of a result that is stored
//! without its text (a memory read masked because a later call of its batch
//! forgot a key it printed, see `agent/turn_engine/batch_forget.rs`).

use super::SqliteMemory;
use super::rf_forget;
use rusqlite::params;

const TOOL_OUTPUT_KINDS: &str = "('tool_output', 'untrusted_external')";
const UNTRUSTED_PREFIX: &str = "untrusted:";

impl SqliteMemory {
    /// Delete the tool-output document `document_id` (and its `untrusted:`
    /// variant) unless a stored tool result row still names it, the same rule
    /// as the forget sweep; rebuilds the document index when one went.
    /// Returns whether a document was deleted.
    pub(super) async fn delete_tool_output_document_impl(&self, document_id: &str) -> anyhow::Result<bool> {
        let pool = self.pool.clone();
        let document = document_id.to_string();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<bool> {
            let conn = pool.write();
            let tx = conn.unchecked_transaction()?;
            let base = document.strip_prefix(UNTRUSTED_PREFIX).unwrap_or(&document).to_owned();
            let mut deleted = false;
            for id in [base.clone(), format!("{UNTRUSTED_PREFIX}{base}")] {
                let tool_output: bool = tx.query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM documents WHERE document_id = ?1 AND source_kind IN {TOOL_OUTPUT_KINDS})"
                    ),
                    params![id],
                    |row| row.get(0),
                )?;
                let quoted: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM message_events
                         WHERE event_type = 'tool.result.recorded' AND json_valid(raw_payload_json)
                           AND json_extract(raw_payload_json, '$.document.document_id') IN (?1, replace(?1, ?2, '')))",
                    params![id, UNTRUSTED_PREFIX],
                    |row| row.get(0),
                )?;
                if tool_output && !quoted {
                    deleted |= rf_forget::delete_document(&tx, &id, true)?;
                }
            }
            rf_forget::rebuild_indexes(&tx, false, deleted)?;
            tx.commit()?;
            Ok(deleted)
        })
        .await?
    }
}
