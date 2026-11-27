//! The effective output of a tool call in its idempotency ledger row.
//!
//! The terminal row `tool.execution.finalized` stores the raw tool output,
//! masked for secrets, and a retry of the same operation replays it. When a
//! `tool_result` hook replaced the text the model read, the row is rewritten
//! so a replay returns the same effective text. One UPDATE by workspace and
//! terminal key; a row whose payload is not the expected JSON is left as is.

use rusqlite::params;

use super::SqliteMemory;

const LEDGER_EVENT_TYPE: &str = "tool.execution.finalized";

/// Rows of an erase-fenced scope are removed by their own erase.
const FENCE_GUARD: &str =
    "NOT EXISTS (SELECT 1 FROM agent_erase_fences f WHERE f.scope_id = message_events.ma_scope_id)";

/// The payload with `outcome.model_content` and, when present as text,
/// `outcome.result.output` set to `text`. `None` when the payload is not a
/// terminal record. Used by the encrypted scope on the opened payload.
pub(crate) fn rewrite_payload(payload: &str, text: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let outcome = value.get_mut("outcome")?.as_object_mut()?;
    if !outcome.get("model_content")?.is_string() {
        return None;
    }
    outcome.insert("model_content".into(), text.into());
    if let Some(output) = outcome
        .get_mut("result")
        .and_then(|result| result.get_mut("output"))
        .filter(|output| output.is_string())
    {
        *output = text.into();
    }
    serde_json::to_string(&value).ok()
}

impl SqliteMemory {
    /// Rewrite the output fields of the ledger row `terminal_key` of
    /// `workspace_id`. With `sealed_payload` the stored payload is replaced by
    /// it (the encrypted scope sealed the rewrite); otherwise the fields are
    /// set in place. Returns whether a row changed.
    pub(super) async fn rewrite_tool_ledger_output_impl(
        &self,
        workspace_id: &str,
        terminal_key: &str,
        text: &str,
        sealed_payload: Option<&str>,
    ) -> anyhow::Result<bool> {
        let pool = self.pool.clone();
        let workspace_id = workspace_id.to_string();
        let terminal_key = terminal_key.to_string();
        let text = text.to_string();
        let sealed = sealed_payload.map(str::to_string);
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<bool> {
            let conn = pool.write();
            let updated = match sealed {
                Some(sealed) => conn.execute(
                    &format!(
                        "UPDATE message_events SET raw_payload_json = ?3
                         WHERE workspace_id = ?1 AND idempotency_key = ?2
                           AND event_type = '{LEDGER_EVENT_TYPE}' AND {FENCE_GUARD}"
                    ),
                    params![workspace_id, terminal_key, sealed],
                )?,
                None => conn.execute(
                    &format!(
                        "UPDATE message_events SET raw_payload_json = CASE
                             WHEN json_type(raw_payload_json, '$.outcome.result.output') = 'text'
                             THEN json_set(raw_payload_json,
                                 '$.outcome.model_content', ?3, '$.outcome.result.output', ?3)
                             ELSE json_set(raw_payload_json, '$.outcome.model_content', ?3) END
                         WHERE workspace_id = ?1 AND idempotency_key = ?2
                           AND event_type = '{LEDGER_EVENT_TYPE}'
                           AND json_valid(raw_payload_json)
                           AND json_type(raw_payload_json, '$.outcome.model_content') = 'text'
                           AND {FENCE_GUARD}"
                    ),
                    params![workspace_id, terminal_key, text],
                )?,
            };
            Ok(updated > 0)
        })
        .await?
    }
}
