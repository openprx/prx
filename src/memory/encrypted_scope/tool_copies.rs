//! Cleartext fields beside the sealed payload of a memory-tool row, so a
//! forget can replace the sealed copy in its own transaction without any key
//! (design Q-E1, option C). The sweep is `sqlite/derived_sweep.rs`
//! `rewrite_sealed_copies`.

use super::{EncryptedScope, TOOL_CALL_EVENT_TYPE, TOOL_RESULT_EVENT_TYPE, TOOL_TERMINAL_EVENT_TYPE};
use crate::security::ramflux_aead::id_hash;

/// Most printed candidates looked up in the store for one row.
const MAX_CANDIDATES: usize = 256;

/// Add `fields` beside the sealed payload: `sealed` is either the ciphertext
/// (stored as `{"encrypted_child_payload": sealed, ...fields}`) or already a
/// child payload wrapper, which keeps its own keys.
pub(super) fn with_fields(sealed: String, fields: serde_json::Map<String, serde_json::Value>) -> String {
    let mut wrapper = match serde_json::from_str::<serde_json::Value>(&sealed) {
        Ok(serde_json::Value::Object(existing)) if existing.contains_key("encrypted_child_payload") => existing,
        _ => {
            let mut fresh = serde_json::Map::new();
            fresh.insert("encrypted_child_payload".to_owned(), serde_json::Value::String(sealed));
            fresh
        }
    };
    wrapper.extend(fields);
    serde_json::Value::Object(wrapper).to_string()
}

impl EncryptedScope {
    /// Seal the payload of the ledger row rewritten after it was written, with
    /// the same wrapper `seal_event_payload` gives it. `content` is the row's
    /// plaintext content.
    pub(super) async fn seal_ledger_payload(
        &self,
        source: &crate::memory::MessageEventSource,
        key: &str,
        payload: &str,
        content: &str,
    ) -> anyhow::Result<String> {
        let sealed = self.seal_payload(source, key, payload)?;
        Ok(
            match self
                .memory_copy_fields(TOOL_TERMINAL_EVENT_TYPE, key, payload, Some(content))
                .await?
            {
                Some(fields) => with_fields(sealed, fields),
                None => sealed,
            },
        )
    }

    /// The keys among `candidates` that name a memory stored in this scope
    /// now. A printed line can be split in several ways and some splits run
    /// into the memory's text; only a split that is a real key of this scope
    /// is kept, so no hash of text is ever stored.
    async fn existing_keys(&self, candidates: Vec<String>) -> anyhow::Result<Vec<String>> {
        let mut keys = Vec::new();
        for candidate in candidates.into_iter().take(MAX_CANDIDATES) {
            if self.inner.get(&self.stored_key(&candidate)).await?.is_some() {
                keys.push(candidate);
            }
        }
        Ok(keys)
    }

    /// The cleartext fields of a memory-tool row, written for a
    /// `tool.result.recorded` row of a memory tool, a `tool.call.recorded` row
    /// of `memory_store` and the `tool.execution.finalized` row of a memory
    /// tool call:
    /// - `memory_key_hashes`: `id_hash` of each key that exists in this scope
    ///   (printed by the tool, or the `memory_store` argument);
    /// - `memory_scope`: `id_hash(scope)`, the value already in clear text in
    ///   the stored key `rf:<scope hash>:<key hash>`;
    /// - `removed_content` and `removed_payload`: the row rewritten the way the
    ///   plaintext sweep rewrites it, sealed by this scope for the row's own
    ///   record ids.
    ///
    /// Never a key's text or any content. `None` for any other row or when no
    /// key is named. `content` is the row's plaintext content (results print
    /// their keys there; a terminal keeps it as it is).
    pub(super) async fn memory_copy_fields(
        &self,
        event_type: &str,
        key: &str,
        payload: &str,
        content: Option<&str>,
    ) -> anyhow::Result<Option<serde_json::Map<String, serde_json::Value>>> {
        use crate::memory::printed_keys::{MEMORY_REMOVED_TEXT, MEMORY_TOOLS, MEMORY_WRITE_TOOLS, printed_memory_keys};
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(payload) else {
            return Ok(None);
        };
        let text_at = |value: &serde_json::Value, pointer: &str| {
            value
                .pointer(pointer)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let memory_tool = |tool: Option<String>, tools: &[&str]| tool.filter(|tool| tools.contains(&tool.as_str()));
        // `printed`: the candidates still to be checked against the store.
        let (printed, written, removed_content) = match event_type {
            TOOL_RESULT_EVENT_TYPE => {
                let (Some(tool), Some(content), Some(object)) = (
                    memory_tool(text_at(&value, "/tool"), &MEMORY_TOOLS),
                    content,
                    value.as_object_mut(),
                ) else {
                    return Ok(None);
                };
                object.insert("document".into(), serde_json::Value::Null);
                object.insert("inline_bytes".into(), MEMORY_REMOVED_TEXT.len().into());
                object.insert("listing_erased".into(), true.into());
                (
                    printed_memory_keys(&tool, content),
                    None,
                    MEMORY_REMOVED_TEXT.to_owned(),
                )
            }
            TOOL_CALL_EVENT_TYPE => {
                let Some(tool) = memory_tool(text_at(&value, "/tool"), &MEMORY_WRITE_TOOLS) else {
                    return Ok(None);
                };
                let Some(memory_key) = text_at(&value, "/arguments/key") else {
                    return Ok(None);
                };
                let Some(arguments) = value
                    .get_mut("arguments")
                    .and_then(serde_json::Value::as_object_mut)
                    .filter(|arguments| arguments.get("content").is_some_and(|content| !content.is_null()))
                else {
                    return Ok(None);
                };
                arguments.insert("content".into(), MEMORY_REMOVED_TEXT.into());
                let removed = format!("{tool} {}", serde_json::Value::Object(arguments.clone()));
                // The argument is a real key: no lookup needed.
                (Vec::new(), Some(memory_key), removed)
            }
            TOOL_TERMINAL_EVENT_TYPE => {
                let (Some(tool), Some(content)) = (
                    memory_tool(text_at(&value, "/reservation/capability"), &MEMORY_TOOLS),
                    content,
                ) else {
                    return Ok(None);
                };
                let mut candidates = Vec::new();
                for pointer in ["/outcome/model_content", "/outcome/result/output"] {
                    if let Some(output) = text_at(&value, pointer) {
                        candidates.extend(printed_memory_keys(&tool, &output));
                    }
                }
                candidates.sort_unstable();
                candidates.dedup();
                let Some(outcome) = value.get_mut("outcome").and_then(serde_json::Value::as_object_mut) else {
                    return Ok(None);
                };
                outcome.insert("model_content".into(), MEMORY_REMOVED_TEXT.into());
                if let Some(result) = outcome.get_mut("result").and_then(serde_json::Value::as_object_mut) {
                    result.insert("output".into(), MEMORY_REMOVED_TEXT.into());
                }
                (candidates, None, content.to_owned())
            }
            _ => return Ok(None),
        };
        let mut keys = self.existing_keys(printed).await?;
        keys.extend(written);
        if keys.is_empty() {
            return Ok(None);
        }
        let mut fields = serde_json::Map::new();
        fields.insert(
            "memory_key_hashes".into(),
            keys.iter().map(|key| id_hash(key)).collect::<Vec<_>>().into(),
        );
        fields.insert("memory_scope".into(), id_hash(&self.scope).into());
        fields.insert("removed_content".into(), self.seal(key, &removed_content)?.into());
        fields.insert(
            "removed_payload".into(),
            self.seal(&format!("{key}:payload"), &value.to_string())?.into(),
        );
        Ok(Some(fields))
    }
}
