//! Memory decorator for one ramflux conversation scope.
//!
//! Content is sealed with the account DEK before it reaches the inner backend.
//! Every read filters to this scope and opens the ciphertext before matching.
//! Bytes that were not sealed for this scope are not returned. Transactional
//! erasure requires a backend that implements the ownership-index contract.

use crate::memory::RAMFLUX_MEMORY_KEY_PREFIX;
use crate::memory::traits::{ConversationTurn, Memory, MemoryCategory, MemoryEntry, MemoryPrincipal};
use crate::security::ramflux_aead::{AccountCrypto, CryptoError, id_hash};
use anyhow::Context;
use async_trait::async_trait;
use base64::Engine;
use std::collections::HashMap;
use std::sync::Arc;

mod transcript_search;

/// Event types of the scheduled-run results read by `newest_owner_events_of_type`.
const SCHEDULED_RESULT_EVENT_TYPES: [&str; 2] = [
    crate::agent::scheduled_result::SCHEDULE_RESULT_EVENT_TYPE,
    crate::context::transcript::SCHEDULE_SEEN_EVENT_TYPE,
];

/// Event type of an attachment row (`context/transcript/tool_rows.rs`).
const ATTACHMENT_EVENT_TYPE: &str = "message.attachment.recorded";

/// Source lists of an attachment payload that stay readable beside the seal.
const ATTACHMENT_SOURCE_LISTS: [&str; 4] = ["memories", "events", "documents", "results"];

/// Store an attachment row as `{"encrypted_child_payload": sealed, "sources": ...}`
/// so the store's provenance triggers can find the rows that quote a deleted
/// memory, document or event. Only the ids (string elements) are copied, never
/// text. A payload that is not a JSON object is stored as sealed. When `sealed`
/// is already a child payload wrapper (`encrypted_child_payload` key), the
/// `sources` key is added to that object instead of wrapping it again.
fn attachment_wrapper(payload: &str, sealed: String) -> String {
    let Some(value) = serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .filter(serde_json::Value::is_object)
    else {
        return sealed;
    };
    let mut sources = serde_json::Map::new();
    for list in ATTACHMENT_SOURCE_LISTS {
        let ids: Vec<serde_json::Value> = match value.pointer(&format!("/sources/{list}")) {
            Some(serde_json::Value::Array(items)) => items.iter().filter(|item| item.is_string()).cloned().collect(),
            Some(item @ serde_json::Value::String(_)) => vec![item.clone()],
            _ => Vec::new(),
        };
        sources.insert(list.to_owned(), serde_json::Value::Array(ids));
    }
    let mut wrapper = match serde_json::from_str::<serde_json::Value>(&sealed) {
        Ok(serde_json::Value::Object(existing)) if existing.contains_key("encrypted_child_payload") => existing,
        _ => {
            let mut fresh = serde_json::Map::new();
            fresh.insert("encrypted_child_payload".to_owned(), serde_json::Value::String(sealed));
            fresh
        }
    };
    wrapper.insert("sources".to_owned(), serde_json::Value::Object(sources));
    serde_json::Value::Object(wrapper).to_string()
}

/// Encrypting view of `inner` for one `agent_account_id/conversation_id` scope.
pub struct EncryptedScope {
    inner: Arc<dyn Memory>,
    crypto: Arc<AccountCrypto>,
    account_id: String,
    scope: String,
}

impl EncryptedScope {
    #[must_use]
    pub fn new(
        inner: Arc<dyn Memory>,
        crypto: Arc<AccountCrypto>,
        account_id: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            crypto,
            account_id: account_id.into(),
            scope: scope.into(),
        }
    }

    fn prefix(&self) -> String {
        format!("{RAMFLUX_MEMORY_KEY_PREFIX}{}:", id_hash(&self.scope))
    }

    fn stored_key(&self, record_id: &str) -> String {
        format!("{}{}", self.prefix(), id_hash(record_id))
    }

    fn seal(&self, record_id: &str, content: &str) -> Result<String, CryptoError> {
        let wrapped = serde_json::json!({ "id": record_id, "scope": self.scope, "body": content }).to_string();
        let blob = self
            .crypto
            .seal(&self.account_id, &self.scope, &id_hash(record_id), wrapped.as_bytes())?;
        if self.crypto.encryption_enabled() {
            Ok(base64::engine::general_purpose::STANDARD.encode(blob))
        } else {
            Ok(String::from_utf8(blob).unwrap_or_default())
        }
    }

    fn open_record(&self, record_id: &str, stored: &str) -> Option<String> {
        let plain = self.open_aad(&id_hash(record_id), stored)?;
        let value: serde_json::Value = serde_json::from_str(&plain).ok()?;
        if value
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|scope| scope != self.scope)
            || (value.get("scope").is_none() && !self.crypto.encryption_enabled())
            || value.get("id")?.as_str()? != record_id
        {
            return None;
        }
        value.get("body").and_then(|item| item.as_str()).map(str::to_string)
    }

    fn open_stored(&self, stored_key: &str, stored: &str) -> Option<(String, String)> {
        if !stored_key.starts_with(&self.prefix()) {
            return None;
        }
        let aad = stored_key.rsplit(':').next()?;
        let plain = self.open_aad(aad, stored)?;
        let value: serde_json::Value = serde_json::from_str(&plain).ok()?;
        if value
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|scope| scope != self.scope)
            || (value.get("scope").is_none() && !self.crypto.encryption_enabled())
        {
            return None;
        }
        let id = value.get("id").and_then(|item| item.as_str())?;
        let body = value.get("body").and_then(|item| item.as_str())?;
        Some((id.to_string(), body.to_string()))
    }

    /// Seal message text. `raw_payload_json` stays intact: ingress stores a
    /// JSON capsule wrapper there and the sqlite receipt parser requires it.
    fn seal_event_content(&self, input: &mut crate::memory::MessageEventInput) -> Result<(), CryptoError> {
        let record_id = input.idempotency_key.clone().unwrap_or_else(|| "event".to_string());
        input.content = self.seal(&record_id, &input.content)?;
        Ok(())
    }

    /// Keep only routing metadata visible to SQLite's transactional child hook.
    /// Source text and the complete original payload remain encrypted.
    fn seal_event_payload(&self, input: &mut crate::memory::MessageEventInput) -> anyhow::Result<()> {
        if let Some(payload) = input.raw_payload_json.take() {
            let key = input.idempotency_key.as_deref().unwrap_or("event");
            let sealed = self.seal_payload(&input.source, key, &payload)?;
            input.raw_payload_json = Some(if input.event_type == ATTACHMENT_EVENT_TYPE {
                attachment_wrapper(&payload, sealed)
            } else {
                sealed
            });
        }
        Ok(())
    }

    fn seal_payload(
        &self,
        source: &crate::memory::MessageEventSource,
        key: &str,
        payload: &str,
    ) -> anyhow::Result<String> {
        let sealed = self.seal(&format!("{key}:payload"), payload)?;
        if !matches!(
            source,
            crate::memory::MessageEventSource::SessionsSpawn | crate::memory::MessageEventSource::SessionWorker
        ) {
            return Ok(sealed);
        }
        let value = serde_json::from_str::<serde_json::Value>(payload).ok();
        let mut wrapper = serde_json::Map::new();
        wrapper.insert("encrypted_child_payload".into(), sealed.into());
        if let Some(record) = value.as_ref().and_then(|value| value.get("child_session_record")) {
            let record: crate::memory::child_sessions::ChildSessionRecord = serde_json::from_value(record.clone())?;
            anyhow::ensure!(record.context.is_empty(), "launch metadata cannot expose child history");
            wrapper.insert("child_session_record".into(), serde_json::to_value(record)?);
        }
        if let Some(status) = value
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(serde_json::Value::as_str)
        {
            wrapper.insert("status".into(), status.into());
        }
        Ok(serde_json::Value::Object(wrapper).to_string())
    }

    fn open_aad(&self, record_hash: &str, stored: &str) -> Option<String> {
        if !self.crypto.encryption_enabled() {
            return Some(stored.to_string());
        }
        let blob = base64::engine::general_purpose::STANDARD
            .decode(stored.as_bytes())
            .ok()?;
        if !blob.starts_with(b"RF1") {
            return None;
        }
        let plain = self
            .crypto
            .open(&self.account_id, &self.scope, record_hash, &blob)
            .ok()?;
        String::from_utf8(plain).ok()
    }

    /// Whether `stored` is a scope record as `seal` writes it in either mode:
    /// RF1 ciphertext (encryption on) or the JSON wrapper with a `scope` key
    /// (encryption off). Both forms are checked whatever the current mode, so a
    /// record written by another scope before the mode changed is never taken
    /// for a plaintext row. Plaintext rows never pass this check.
    fn looks_sealed(&self, stored: &str) -> bool {
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(stored.as_bytes())
            .is_ok_and(|blob| blob.starts_with(b"RF1"));
        ciphertext || serde_json::from_str::<serde_json::Value>(stored).is_ok_and(|value| value.get("scope").is_some())
    }

    fn owns_event(&self, event: &crate::memory::MessageEvent) -> bool {
        self.open_record(event.idempotency_key.as_deref().unwrap_or("event"), &event.content)
            .is_some()
    }

    fn open_memory_event(&self, mut event: crate::memory::MemoryEvent) -> Option<crate::memory::MemoryEvent> {
        let plain = self.open_record(
            &format!("memory-event:{}", event.event_id),
            event.payload_json.as_deref()?,
        )?;
        let original: crate::memory::MemoryEventInput = serde_json::from_str(&plain).ok()?;
        event.subject_id = original.subject_id;
        event.payload_json = original.payload_json;
        Some(event)
    }

    fn open_draft(&self, mut draft: crate::memory::MemoryDraft) -> Option<crate::memory::MemoryDraft> {
        if !draft.key.starts_with(&self.prefix()) {
            return None;
        }
        let (key, content) = self.open_stored(&draft.key, &draft.content)?;
        draft.key = key;
        draft.content = content;
        let plain = self.open_record(
            &format!("draft-meta:{}", draft.draft_id),
            draft.payload_json.as_deref()?,
        )?;
        let payload: serde_json::Value = serde_json::from_str(&plain).ok()?;
        let id = payload.get("draft_id")?.as_str()?.to_owned();
        draft.session_key = payload
            .get("session_key")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        draft.payload_json = payload
            .get("payload_json")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        draft.draft_id = id;
        Some(draft)
    }

    fn reveal(&self, mut event: crate::memory::MessageEvent) -> crate::memory::MessageEvent {
        let record_id = event.idempotency_key.clone().unwrap_or_else(|| "event".to_string());
        if let Some(plain) = self.open_record(&record_id, &event.content) {
            event.content = plain;
        }
        if let Some(payload) = event.raw_payload_json.as_deref()
            && let Ok(wrapper) = serde_json::from_str::<serde_json::Value>(payload)
            && let Some(stored) = wrapper
                .get("encrypted_child_payload")
                .and_then(serde_json::Value::as_str)
            && let Some(plain) = self.open_record(&format!("{record_id}:payload"), stored)
        {
            event.raw_payload_json = Some(plain);
        }
        if let Some(stored) = event.raw_payload_json.as_deref()
            && let Some(plain) = self.open_record(&format!("{record_id}:payload"), stored)
        {
            event.raw_payload_json = Some(plain);
        }
        if let Some(payload) = event.raw_payload_json.as_deref()
            && let Ok(mut wrapper) = serde_json::from_str::<serde_json::Value>(payload)
            && let Some(stored) = wrapper.get("original_payload_json").and_then(serde_json::Value::as_str)
            && let Some(plain) = self.open_record(&format!("{record_id}:payload"), stored)
        {
            if let Some(field) = wrapper.get_mut("original_payload_json") {
                *field = serde_json::Value::String(plain);
            }
            event.raw_payload_json = Some(wrapper.to_string());
        }
        event
    }
}

impl EncryptedScope {
    /// The `owner_id` every sealed row of this account carries. `sealed_write`
    /// stamps it on write and `newest_core_entries` reads with it, so the two
    /// cannot disagree on the owner.
    fn sealed_owner_id(&self) -> Result<String, CryptoError> {
        self.crypto
            .mac(&self.account_id, "memory-owner", self.account_id.as_bytes())
    }

    /// The one sealed write shared by `store` and both session-snapshot
    /// paths, so they cannot drift apart: the stored key, the sealed content,
    /// the session MAC and the metadata. The scope's `channel` and `owner_id`
    /// win over any caller-supplied metadata.
    fn sealed_write(
        &self,
        key: &str,
        content: &str,
        metadata: crate::memory::MemoryStoreMetadata,
    ) -> anyhow::Result<(String, String, String, crate::memory::MemoryStoreMetadata)> {
        let sealed = self.seal(key, content)?;
        let session = self
            .crypto
            .mac(&self.account_id, "memory-session", self.scope.as_bytes())?;
        let owner = self.sealed_owner_id()?;
        // Sealed payloads carry attribution without entering plaintext topic
        // classification, which would create a copy outside message erasure.
        let metadata = crate::memory::MemoryStoreMetadata {
            channel: Some("ramflux".to_owned()),
            owner_id: Some(owner),
            ..metadata
        };
        Ok((self.stored_key(key), sealed, session, metadata))
    }
}

#[async_trait]
impl Memory for EncryptedScope {
    fn requires_scoped_execution(&self) -> bool {
        true
    }
    fn with_erasure_origin(&self, origin: crate::memory::erasure::ErasureOrigin) -> anyhow::Result<Arc<dyn Memory>> {
        Ok(Arc::new(Self::new(
            self.inner.with_erasure_origin(origin)?,
            Arc::clone(&self.crypto),
            self.account_id.clone(),
            self.scope.clone(),
        )))
    }
    fn erasure_store(&self) -> anyhow::Result<crate::memory::erasure::SharedErasureStore> {
        self.inner.erasure_store()
    }
    fn with_write_provenance(
        &self,
        provenance: crate::memory::write_provenance::WriteProvenance,
    ) -> anyhow::Result<Arc<dyn Memory>> {
        Ok(Arc::new(Self::new(
            self.inner.with_write_provenance(provenance)?,
            Arc::clone(&self.crypto),
            self.account_id.clone(),
            self.scope.clone(),
        )))
    }

    fn recovery_repository(&self) -> Option<&dyn crate::memory::recovery_authority::RecoveryRepository> {
        self.inner.recovery_repository()
    }

    /// Never delegated: an incarnation belongs to the plain backend's scope.
    fn agent_registry(&self) -> Option<&dyn crate::memory::agent_registry::AgentRegistry> {
        None
    }

    fn supports_agent_scope(&self) -> bool {
        false
    }

    fn name(&self) -> &str {
        // The ledger and event recorder select a backend by this name. The
        // inner store is that backend; this view only seals the bytes it writes.
        self.inner.name()
    }

    /// A re-store of unchanged content keeps the stored ciphertext. A fresh
    /// nonce would make every re-store look like a content change to the inner
    /// backend, which drops the session prefix rows of the owner.
    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let (stored_key, mut sealed, session, metadata) =
            self.sealed_write(key, content, crate::memory::MemoryStoreMetadata::default())?;
        // A failed read of the current row only skips the reuse: the store then
        // writes the fresh seal, as it did before the identical re-store check.
        if let Ok(Some(current)) = self.inner.get(&stored_key).await
            && self.open_record(key, &current.content).as_deref() == Some(content)
        {
            sealed = current.content;
        }
        self.inner
            .store_with_metadata(&stored_key, &sealed, category, Some(&session), metadata)
            .await
    }

    /// Snapshots take the same sealed path as `store`; the inner backend
    /// receives no content filter for them.
    async fn store_session_snapshot(&self, key: &str, content: &str, session_id: Option<&str>) -> anyhow::Result<()> {
        self.store_session_snapshot_with_metadata(
            key,
            content,
            session_id,
            crate::memory::MemoryStoreMetadata::default(),
        )
        .await
    }

    async fn store_session_snapshot_with_metadata(
        &self,
        key: &str,
        content: &str,
        _session_id: Option<&str>,
        metadata: crate::memory::MemoryStoreMetadata,
    ) -> anyhow::Result<()> {
        let (stored_key, sealed, session, metadata) = self.sealed_write(key, content, metadata)?;
        self.inner
            .store_session_snapshot_with_metadata(&stored_key, &sealed, Some(&session), metadata)
            .await
    }

    async fn recall(&self, query: &str, limit: usize, _session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let entries = self.inner.list_key_prefix(&self.prefix(), usize::MAX).await?;
        let needle = query.to_lowercase();
        let mut matched = Vec::new();
        for mut entry in entries {
            let Some((record_id, plain)) = self.open_stored(&entry.key, &entry.content) else {
                continue;
            };
            if !needle.is_empty() && !plain.to_lowercase().contains(&needle) {
                continue;
            }
            entry.content = plain;
            entry.key = record_id;
            entry.session_id = Some(self.scope.clone());
            matched.push(entry);
            if matched.len() >= limit {
                break;
            }
        }
        Ok(matched)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let Some(mut entry) = self.inner.get(&self.stored_key(key)).await? else {
            return Ok(None);
        };
        let Some(plain) = self.open_record(key, &entry.content) else {
            return Ok(None);
        };
        entry.content = plain;
        entry.key = key.to_string();
        entry.session_id = Some(self.scope.clone());
        Ok(Some(entry))
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let entries = self.inner.list_key_prefix(&self.prefix(), usize::MAX).await?;
        let mut own = Vec::new();
        for mut entry in entries {
            let Some((record_id, plain)) = self.open_stored(&entry.key, &entry.content) else {
                continue;
            };
            entry.content = plain;
            entry.key = record_id;
            entry.session_id = Some(self.scope.clone());
            own.push(entry);
        }
        if let Some(category) = category {
            own.retain(|entry| &entry.category == category);
        }
        Ok(own)
    }

    async fn list_key_prefix(&self, prefix: &str, limit: usize) -> anyhow::Result<Vec<MemoryEntry>> {
        let entries = self.inner.list_key_prefix(&self.prefix(), usize::MAX).await?;
        let mut own = Vec::new();
        for mut entry in entries {
            let Some((record_id, plain)) = self.open_stored(&entry.key, &entry.content) else {
                continue;
            };
            if record_id.starts_with(prefix) {
                entry.content = plain;
                entry.key = record_id;
                own.push(entry);
            }
        }
        own.truncate(limit);
        Ok(own)
    }

    /// The inner store's newest core rows, narrowed to this scope and decrypted
    /// the way `list_key_prefix` does. Sealed core rows carry the account's
    /// memory-owner MAC, no workspace and no agent partition (`sealed_write`), so
    /// the inner read uses that owner instead of the principal's resolved owner.
    /// The inner read is unbounded because rows of other scopes are dropped after
    /// it, as `list_key_prefix` does.
    async fn newest_core_entries(&self, principal: &MemoryPrincipal, limit: usize) -> anyhow::Result<Vec<MemoryEntry>> {
        let sealed = MemoryPrincipal {
            owner_id: Some(self.sealed_owner_id()?),
            agent_partition: None,
            ..principal.clone()
        };
        let entries = self.inner.newest_core_entries(&sealed, usize::MAX).await?;
        let mut own = Vec::new();
        for mut entry in entries {
            let Some((record_id, plain)) = self.open_stored(&entry.key, &entry.content) else {
                continue;
            };
            entry.content = plain;
            entry.key = record_id;
            own.push(entry);
        }
        own.truncate(limit);
        Ok(own)
    }

    fn persisted_key(&self, key: &str) -> String {
        self.stored_key(key)
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.inner.forget(&self.stored_key(key)).await
    }

    async fn forget_message_idempotency(&self, idempotency_key: &str) -> anyhow::Result<usize> {
        self.inner.forget_message_idempotency(idempotency_key).await
    }

    async fn delete_session_prefix_row(&self, row_id: i64) -> anyhow::Result<usize> {
        self.inner.delete_session_prefix_row(row_id).await
    }

    async fn checkpoint_wal(&self) -> anyhow::Result<()> {
        self.inner.checkpoint_wal().await
    }

    async fn forget_key_prefix(&self, prefix: &str) -> anyhow::Result<usize> {
        let entries = self.list_key_prefix(prefix, usize::MAX).await?;
        let mut removed = 0usize;
        for entry in entries {
            if self.forget(&entry.key).await? {
                removed = removed.saturating_add(1);
            }
        }
        Ok(removed)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(self.list(None, None).await?.len())
    }

    async fn reindex(&self) -> anyhow::Result<usize> {
        Err(crate::memory::erasure::Unsupported("scoped reindex").into())
    }

    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }

    fn supports_document_ingest(&self) -> bool {
        false
    }

    async fn append_message_event(
        &self,
        mut input: crate::memory::MessageEventInput,
    ) -> anyhow::Result<crate::memory::MessageEvent> {
        self.seal_event_content(&mut input)?;
        // Ingress keeps its capsule wrapper parseable. Every other event,
        // including the terminal marker, seals the payload so reply text is
        // not stored beside the ciphertext.
        self.seal_event_payload(&mut input)?;
        let event = self.inner.append_message_event(input).await?;
        Ok(self.reveal(event))
    }

    async fn rewrite_tool_ledger_output(
        &self,
        workspace_id: &str,
        terminal_key: &str,
        effective_text: &str,
        sealed_payload: Option<&str>,
    ) -> anyhow::Result<bool> {
        // The ledger payload is sealed here: open it, rewrite, seal it again
        // through the scope's own path.
        let _ = sealed_payload;
        let Some(stored) = self
            .inner
            .find_message_event_by_idempotency_key(workspace_id, terminal_key)
            .await?
        else {
            return Ok(false);
        };
        let source = stored.source.clone();
        let opened = self.reveal(stored);
        let Some(rewritten) = opened
            .raw_payload_json
            .as_deref()
            .and_then(|payload| crate::memory::sqlite::ledger_effective::rewrite_payload(payload, effective_text))
        else {
            return Ok(false);
        };
        let sealed = self.seal_payload(&source, terminal_key, &rewritten)?;
        self.inner
            .rewrite_tool_ledger_output(workspace_id, terminal_key, effective_text, Some(&sealed))
            .await
    }

    async fn append_message_event_until(
        &self,
        mut input: crate::memory::MessageEventInput,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::MessageEvent> {
        self.seal_event_content(&mut input)?;
        self.seal_event_payload(&mut input)?;
        Ok(self.reveal(self.inner.append_message_event_until(input, deadline).await?))
    }

    async fn project_console_message_until(
        &self,
        event: &crate::memory::MessageEvent,
        deadline: std::time::Instant,
    ) -> anyhow::Result<()> {
        let mut event = event.clone();
        let key = event.idempotency_key.as_deref().unwrap_or("event");
        event.content = self.seal(key, &event.content)?;
        if let Some(payload) = event.raw_payload_json.take() {
            event.raw_payload_json = Some(self.seal_payload(&event.source, key, &payload)?);
        }
        self.inner.project_console_message_until(&event, deadline).await
    }

    async fn list_message_events_since(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
        principal.reject_named_scope()?;
        Ok(self
            .inner
            .list_message_events_since(principal, after_id, limit)
            .await?
            .into_iter()
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event))
            .collect())
    }

    async fn list_message_events_recent(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
        Ok(self
            .inner
            .list_message_events_recent(principal, limit)
            .await?
            .into_iter()
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event))
            .collect())
    }

    async fn list_worker_launch_events(
        &self,
        workspace_id: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
        Ok(self
            .inner
            .list_worker_launch_events(workspace_id, after_id, limit)
            .await?
            .into_iter()
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event))
            .collect())
    }

    async fn list_owned_message_events_by_type(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_type: &str,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
        Ok(self
            .inner
            .list_owned_message_events_by_type(workspace_id, owner_id, event_type, after_id, limit)
            .await?
            .into_iter()
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event))
            .collect())
    }

    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<crate::memory::MessageEvent>> {
        let event = self
            .inner
            .find_message_event_by_idempotency_key(workspace_id, idempotency_key)
            .await?;
        Ok(event
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event)))
    }

    async fn find_owned_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        owner_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<crate::memory::MessageEvent>> {
        let event = self
            .inner
            .find_owned_message_event_by_idempotency_key(workspace_id, owner_id, idempotency_key)
            .await?;
        Ok(event
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event)))
    }

    /// Statuses of the final rows of the runs, read from the inner rows: a row
    /// this scope can open is revealed; a sealed row of another scope is
    /// dropped; a plaintext row (a turn of a plain surface, which a seen mark
    /// may name) is read as stored. Only statuses leave, never content.
    async fn turn_final_statuses(
        &self,
        workspace_id: &str,
        owner_id: &str,
        run_ids: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, String>> {
        let rows = self.inner.turn_final_rows(workspace_id, owner_id, run_ids).await?;
        Ok(crate::memory::traits::final_statuses_of(rows.into_iter().filter_map(
            |event| {
                if self.owns_event(&event) {
                    Some(self.reveal(event))
                } else if self.looks_sealed(&event.content) {
                    None
                } else {
                    Some(event)
                }
            },
        )))
    }

    async fn find_owned_message_event(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_id: &str,
    ) -> anyhow::Result<Option<crate::memory::MessageEvent>> {
        let event = self
            .inner
            .find_owned_message_event(workspace_id, owner_id, event_id)
            .await?;
        Ok(event
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event)))
    }

    async fn upsert_chat_profile_metadata(
        &self,
        channel: &str,
        chat_id: &str,
        chat_kind: &str,
        title: Option<&str>,
    ) -> anyhow::Result<()> {
        let id = format!(
            "{}chat:{}",
            self.prefix(),
            crate::security::ramflux_aead::id_hash(&serde_json::to_string(&(channel, chat_id))?)
        );
        self.inner
            .upsert_chat_profile_metadata(channel, &id, chat_kind, title)
            .await
    }

    async fn get_chat_profile(
        &self,
        channel: &str,
        chat_id: &str,
    ) -> anyhow::Result<Option<crate::memory::ChatProfile>> {
        let id = format!(
            "{}chat:{}",
            self.prefix(),
            crate::security::ramflux_aead::id_hash(&serde_json::to_string(&(channel, chat_id))?)
        );
        Ok(self.inner.get_chat_profile(channel, &id).await?.map(|mut profile| {
            profile.chat_id = chat_id.to_owned();
            profile
        }))
    }

    async fn append_conversation_turn(
        &self,
        session_key: &str,
        _channel: &str,
        _sender: &str,
        role: &str,
        content: &str,
        timestamp: Option<&str>,
        message_id: Option<&str>,
        _owner_id: Option<&str>,
        _agent_partition: Option<&crate::memory::AgentPartition>,
    ) -> anyhow::Result<()> {
        let record = message_id.filter(|value| !value.is_empty()).unwrap_or(session_key);
        let key = format!("convturn:{session_key}:{role}:{record}");
        let payload = serde_json::json!({
            "role": role,
            "content": content,
            "session": session_key,
            "message_id": message_id,
            "timestamp": timestamp.map(str::to_owned).unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        });
        let encoded = payload.to_string();
        self.store(&key, &encoded, MemoryCategory::Conversation, Some(session_key))
            .await
    }

    async fn load_recent_conversation_histories(
        &self,
        _principal: &crate::memory::MemoryPrincipal,
        max_turns_per_session: usize,
        max_sessions: usize,
    ) -> anyhow::Result<HashMap<String, Vec<ConversationTurn>>> {
        let entries = self.list(None, None).await?;
        let mut out: HashMap<String, Vec<ConversationTurn>> = HashMap::new();
        for entry in entries {
            if !entry.key.starts_with("convturn:") {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&entry.content) else {
                continue;
            };
            let session = value.get("session").and_then(|item| item.as_str()).unwrap_or("");
            if session.is_empty() {
                continue;
            }
            if out.len() >= max_sessions && !out.contains_key(session) {
                continue;
            }
            let turns = out.entry(session.to_string()).or_default();
            turns.push(ConversationTurn {
                id: 0,
                session_key: session.to_string(),
                role: value
                    .get("role")
                    .and_then(|item| item.as_str())
                    .unwrap_or("user")
                    .to_string(),
                content: value
                    .get("content")
                    .and_then(|item| item.as_str())
                    .unwrap_or("")
                    .to_string(),
                timestamp: value
                    .get("timestamp")
                    .and_then(|item| item.as_str())
                    .unwrap_or("")
                    .to_string(),
                message_id: value
                    .get("message_id")
                    .and_then(|item| item.as_str())
                    .map(str::to_string),
                message_event_id: None,
            });
        }
        for turns in out.values_mut() {
            turns.sort_by(|left, right| left.timestamp.cmp(&right.timestamp));
            let excess = turns.len().saturating_sub(max_turns_per_session);
            turns.drain(..excess);
        }
        Ok(out)
    }

    async fn retrieve_decision_memory(
        &self,
        query: &str,
        limit: u8,
        context: &crate::memory::MemoryWriteContext,
        session_id: Option<&str>,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::traits::DecisionMemoryHit>> {
        let _ = context;
        anyhow::ensure!(
            query.len() <= 1024 && (1..=5).contains(&limit),
            "invalid decision Memory bounds"
        );
        anyhow::ensure!(
            session_id.is_none_or(|id| !id.is_empty() && id.len() <= 512),
            "invalid decision Memory session"
        );
        crate::memory::ingress_recovery::remaining(deadline)?;
        let entries = self.recall(query, usize::from(limit), None).await?;
        crate::memory::ingress_recovery::remaining(deadline)?;
        Ok(entries
            .into_iter()
            .map(|entry| crate::memory::traits::DecisionMemoryHit {
                id: entry.id,
                key: entry.key,
                category: entry.category,
                session_id: entry.session_id,
                content: entry.content,
                created_at: entry.timestamp,
                source_event_id: entry.source_event_id,
            })
            .collect())
    }

    async fn retrieve_decision_documents(
        &self,
        query: &str,
        limit: u8,
        principal: &crate::memory::MemoryPrincipal,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::traits::DecisionDocumentHit>> {
        let _ = (query, limit, principal, deadline);
        Err(crate::memory::erasure::Unsupported("scoped document retrieval").into())
    }

    async fn retrieve_decision_session(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        before_event_id: &str,
        limit: u8,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::traits::DecisionSessionHit>> {
        crate::memory::ingress_recovery::remaining(deadline)?;
        let events = self.list_message_events_recent(principal, usize::MAX).await?;
        let cutoff = events
            .iter()
            .find(|event| event.event_id == before_event_id)
            .map_or(i64::MAX, |event| event.id);
        let mut hits: Vec<_> = events
            .into_iter()
            .filter(|event| event.id < cutoff)
            .map(|event| crate::memory::traits::DecisionSessionHit {
                event_id: event.event_id,
                role: event.role,
                content: event.content,
                created_at: event.created_at,
            })
            .collect();
        if hits.len() > usize::from(limit) {
            hits.drain(..hits.len().saturating_sub(usize::from(limit)));
        }
        crate::memory::ingress_recovery::remaining(deadline)?;
        Ok(hits)
    }

    async fn read_router_history(
        &self,
        evidence: crate::memory::RouterHistoryReadEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::RouterHistoryRead> {
        self.inner.read_router_history(evidence, deadline).await
    }

    async fn commit_router_history(
        &self,
        evidence: &crate::memory::RouterHistoryWriteEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::RouterHistoryCommitReceipt> {
        self.inner.commit_router_history(evidence, deadline).await
    }

    async fn read_router_learning(
        &self,
        evidence: &crate::memory::traits::RouterLearningEvidence,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::traits::RouterLearningRead> {
        self.inner.read_router_learning(evidence, deadline).await
    }

    async fn commit_router_learning(
        &self,
        evidence: &crate::memory::traits::RouterLearningEvidence,
        expected_version: i64,
        next: &crate::memory::traits::RouterLearningSnapshot,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::traits::RouterLearningCommit> {
        self.inner
            .commit_router_learning(evidence, expected_version, next, deadline)
            .await
    }

    async fn count_rehearsal_hits(
        &self,
        query: &str,
        limit: u8,
        context: &crate::memory::MemoryWriteContext,
        deadline: std::time::Instant,
    ) -> anyhow::Result<u32> {
        Ok(u32::try_from(
            self.retrieve_decision_memory(query, limit, context, None, deadline)
                .await?
                .len(),
        )?)
    }

    async fn project_console_message(&self, event: &crate::memory::MessageEvent) -> anyhow::Result<()> {
        let mut sealed = event.clone();
        let key = sealed.idempotency_key.as_deref().unwrap_or("event");
        sealed.content = self.seal(key, &sealed.content)?;
        if let Some(payload) = &sealed.raw_payload_json {
            sealed.raw_payload_json = Some(self.seal(&format!("{key}:payload"), payload)?);
        }
        self.inner.project_console_message(&sealed).await
    }

    async fn acquire_ingress_lifetime(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<std::sync::Arc<dyn crate::memory::ingress_recovery::IngressLifetime>> {
        self.inner.acquire_ingress_lifetime(workspace_id, token, deadline).await
    }

    async fn append_ingress_input(
        &self,
        input: crate::memory::MessageEventInput,
        capsule: crate::memory::ingress_recovery::IngressCapsule,
        lifetime: std::sync::Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        deadline: std::time::Instant,
        attestation: Option<crate::memory::ingress_recovery::IngressAttestation>,
    ) -> anyhow::Result<crate::memory::ingress_recovery::IngressAppendOutcome> {
        let mut input = input;
        let mut attestation = attestation;
        if let Some(attestation) = attestation.as_mut().filter(|attestation| !attestation.is_verified()) {
            // This is the first layer holding the plaintext input: verify the
            // transport tag before sealing, so SQLite never recomputes over ciphertext.
            let repository = self
                .inner
                .recovery_repository()
                .ok_or(crate::memory::ingress_recovery::IngressAttestationError::Unsupported)?;
            crate::memory::ingress_recovery::verify_transport(repository, &input, &capsule, attestation)?;
        }
        self.seal_event_content(&mut input)?;
        if let Some(payload) = input.raw_payload_json.take() {
            let mut wrapper: serde_json::Value = serde_json::from_str(&payload)?;
            let key = input.idempotency_key.as_deref().unwrap_or("event");
            if let Some(original) = wrapper.get("original_payload_json").and_then(serde_json::Value::as_str) {
                let sealed = self.seal(&format!("{key}:payload"), original)?;
                let field = wrapper
                    .get_mut("original_payload_json")
                    .ok_or_else(|| anyhow::anyhow!("ingress payload field missing"))?;
                *field = serde_json::Value::String(sealed);
            }
            input.raw_payload_json = Some(wrapper.to_string());
        }
        let outcome = self
            .inner
            .append_ingress_input(input, capsule, lifetime, deadline, attestation)
            .await?;
        Ok(match outcome {
            crate::memory::ingress_recovery::IngressAppendOutcome::Inserted(event) => {
                crate::memory::ingress_recovery::IngressAppendOutcome::Inserted(self.reveal(event))
            }
            crate::memory::ingress_recovery::IngressAppendOutcome::AlreadyAccepted(event) => {
                crate::memory::ingress_recovery::IngressAppendOutcome::AlreadyAccepted(self.reveal(event))
            }
            crate::memory::ingress_recovery::IngressAppendOutcome::Tombstoned => {
                crate::memory::ingress_recovery::IngressAppendOutcome::Tombstoned
            }
        })
    }

    async fn list_ingress_recovery_candidates(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        current: std::sync::Arc<dyn crate::memory::ingress_recovery::IngressLifetime>,
        phase: crate::memory::ingress_recovery::IngressRecoveryScan,
        after_id: i64,
        limit: usize,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Vec<crate::memory::ingress_recovery::IngressRecoveryCandidate>> {
        let mut cursor = after_id;
        let mut visible = Vec::new();
        loop {
            let page = self
                .inner
                .list_ingress_recovery_candidates(scope, current.clone(), phase, cursor, limit, deadline)
                .await?;
            let exhausted = page.len() < limit;
            let Some(last) = page.last() else {
                break;
            };
            cursor = last.cursor;
            for candidate in page {
                let input = self
                    .inner
                    .read_ingress_event(
                        scope,
                        &crate::memory::ingress_recovery::IngressEventLookup::EventId(
                            candidate.capsule.input_event_id.clone(),
                        ),
                        deadline,
                    )
                    .await?;
                if input.as_ref().is_some_and(|event| self.owns_event(event)) {
                    visible.push(candidate);
                    if visible.len() == limit {
                        return Ok(visible);
                    }
                }
            }
            if exhausted {
                break;
            }
        }
        Ok(visible)
    }

    async fn read_ingress_event(
        &self,
        scope: &crate::memory::ingress_recovery::IngressRecoveryScope,
        lookup: &crate::memory::ingress_recovery::IngressEventLookup,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<crate::memory::MessageEvent>> {
        let event = self.inner.read_ingress_event(scope, lookup, deadline).await?;
        Ok(event
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event)))
    }

    async fn claim_ingress_recovery(
        &self,
        workspace_id: &str,
        token: &str,
        deadline: std::time::Instant,
    ) -> anyhow::Result<Option<crate::memory::ingress_recovery::IngressRecoveryClaim>> {
        self.inner.claim_ingress_recovery(workspace_id, token, deadline).await
    }

    async fn commit_ingress_recovery(
        &self,
        mut commit: crate::memory::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<crate::memory::MessageEvent> {
        self.seal_event_content(&mut commit.evidence)?;
        let event = self.inner.commit_ingress_recovery(commit).await?;
        Ok(self.reveal(event))
    }

    async fn commit_orphaned_ingress_recovery(
        &self,
        mut commit: crate::memory::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<crate::memory::MessageEvent> {
        self.seal_event_content(&mut commit.evidence)?;
        let event = self.inner.commit_orphaned_ingress_recovery(commit).await?;
        Ok(self.reveal(event))
    }

    async fn commit_owned_admission_failure(
        &self,
        mut event: crate::memory::MessageEventInput,
        mut ingress: crate::memory::ingress_recovery::IngressRecoveryCommit,
    ) -> anyhow::Result<crate::memory::MessageEvent> {
        // A replay keeps the stored ciphertext so each receipt compares equal.
        // The ingress evidence keeps its parseable binding payload; the record
        // seals its payload like every other event.
        let stored_event = match event.idempotency_key.as_deref() {
            Some(key) => {
                self.inner
                    .find_message_event_by_idempotency_key(&event.workspace_id, key)
                    .await?
            }
            None => None,
        };
        match stored_event {
            Some(stored) if self.reveal(stored.clone()).content == event.content => {
                let plain_payload = self.reveal(stored.clone()).raw_payload_json;
                event.content = stored.content;
                if plain_payload == event.raw_payload_json {
                    event.raw_payload_json = stored.raw_payload_json;
                }
            }
            _ => {
                self.seal_event_content(&mut event)?;
                if let Some(payload) = event.raw_payload_json.take() {
                    let key = event.idempotency_key.as_deref().unwrap_or("event");
                    event.raw_payload_json = Some(self.seal_payload(&event.source, key, &payload)?);
                }
            }
        }
        let stored_evidence = match ingress.evidence.idempotency_key.as_deref() {
            Some(key) => {
                self.inner
                    .find_message_event_by_idempotency_key(&ingress.evidence.workspace_id, key)
                    .await?
            }
            None => None,
        };
        match stored_evidence {
            Some(stored) if self.reveal(stored.clone()).content == ingress.evidence.content => {
                ingress.evidence.content = stored.content;
            }
            _ => self.seal_event_content(&mut ingress.evidence)?,
        }
        let stored = self.inner.commit_owned_admission_failure(event, ingress).await?;
        Ok(self.reveal(stored))
    }

    async fn commit_recovery_terminal(
        &self,
        mut commit: crate::memory::recovery_authority::RecoveryTerminalCommit,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::recovery_authority::RecoveryTerminalOutcome> {
        let final_index = commit.events.len().saturating_sub(1);
        let mut stored_final = None;
        for (index, event) in commit.events.iter_mut().enumerate() {
            let stored = if let Some(key) = event.idempotency_key.as_deref() {
                self.inner
                    .find_message_event_by_idempotency_key(&event.workspace_id, key)
                    .await?
            } else {
                None
            };
            if let Some(stored) = stored.as_ref() {
                let plain = self.reveal(stored.clone());
                if plain.content != event.content {
                    return Ok(crate::memory::recovery_authority::RecoveryTerminalOutcome::Conflict);
                }
                event.content.clone_from(&stored.content);
            } else {
                self.seal_event_content(event)?;
            }
            if index == final_index {
                stored_final = stored;
            } else if let Some(payload) = event.raw_payload_json.take() {
                let key = event.idempotency_key.as_deref().unwrap_or("event");
                event.raw_payload_json = Some(match stored.as_ref() {
                    Some(stored) if self.reveal(stored.clone()).raw_payload_json.as_deref() == Some(&payload) => stored
                        .raw_payload_json
                        .clone()
                        .context("stored recovery payload missing")?,
                    _ => self.seal_payload(&event.source, key, &payload)?,
                });
            }
        }
        if let Some(ingress) = commit.ingress.as_mut() {
            // A replay keeps the stored ciphertext so the receipt compares equal.
            let stored = match ingress.evidence.idempotency_key.as_deref() {
                Some(key) => {
                    self.inner
                        .find_message_event_by_idempotency_key(&ingress.evidence.workspace_id, key)
                        .await?
                }
                None => None,
            };
            match stored {
                Some(stored) if self.reveal(stored.clone()).content == ingress.evidence.content => {
                    ingress.evidence.content.clone_from(&stored.content);
                }
                _ => self.seal_event_content(&mut ingress.evidence)?,
            }
        }
        let reveal = Self {
            inner: Arc::clone(&self.inner),
            crypto: Arc::clone(&self.crypto),
            account_id: self.account_id.clone(),
            scope: self.scope.clone(),
        };
        let outcome = crate::memory::sqlite::map_after_prefix(
            move |hook| {
                Box::new(move |prefix, final_input| {
                    let revealed: Vec<_> = prefix.iter().cloned().map(|event| reveal.reveal(event)).collect();
                    if let Some(hook) = hook {
                        hook(&revealed, final_input)?;
                    }
                    if let Some(payload) = final_input.raw_payload_json.take() {
                        let key = final_input.idempotency_key.as_deref().unwrap_or("event");
                        final_input.raw_payload_json = Some(match stored_final.as_ref() {
                            Some(stored)
                                if reveal.reveal(stored.clone()).raw_payload_json.as_deref() == Some(&payload) =>
                            {
                                stored
                                    .raw_payload_json
                                    .clone()
                                    .context("stored recovery payload missing")?
                            }
                            _ => reveal.seal_payload(&final_input.source, key, &payload)?,
                        });
                    }
                    Ok(())
                })
            },
            self.inner.commit_recovery_terminal(commit, deadline),
        )
        .await?;
        Ok(match outcome {
            crate::memory::recovery_authority::RecoveryTerminalOutcome::Committed { receipt, events } => {
                crate::memory::recovery_authority::RecoveryTerminalOutcome::Committed {
                    receipt,
                    events: events.into_iter().map(|event| self.reveal(event)).collect(),
                }
            }
            crate::memory::recovery_authority::RecoveryTerminalOutcome::Replayed { receipt, events } => {
                crate::memory::recovery_authority::RecoveryTerminalOutcome::Replayed {
                    receipt,
                    events: events.into_iter().map(|event| self.reveal(event)).collect(),
                }
            }
            crate::memory::recovery_authority::RecoveryTerminalOutcome::Conflict => {
                crate::memory::recovery_authority::RecoveryTerminalOutcome::Conflict
            }
        })
    }

    async fn load_recent_shared_context(
        &self,
        query: crate::memory::SharedContextQuery,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
        let events = self.inner.load_recent_shared_context(query).await?;
        Ok(events
            .into_iter()
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event))
            .collect())
    }

    async fn load_recent_session_context(
        &self,
        query: crate::memory::SessionContextQuery,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
        let events = self.inner.load_recent_session_context(query).await?;
        Ok(events
            .into_iter()
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event))
            .collect())
    }

    async fn newest_session_events_of_type(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        session_key: &str,
        event_type: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
        // Rows of this event type written by other owners stay in the store:
        // only rows this scope can open are returned, revealed.
        let events = self
            .inner
            .newest_session_events_of_type(principal, session_key, event_type, limit)
            .await?;
        Ok(events
            .into_iter()
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event))
            .collect())
    }

    async fn session_events_of_type_before(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        session_key: &str,
        event_type: &str,
        before_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
        // As `newest_session_events_of_type`: only rows this scope can open are
        // returned, revealed.
        let events = self
            .inner
            .session_events_of_type_before(principal, session_key, event_type, before_id, limit)
            .await?;
        Ok(events
            .into_iter()
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event))
            .collect())
    }

    async fn session_transcript_rows(
        &self,
        principal: &MemoryPrincipal,
        session_key: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
        // Only rows this scope can open are returned, revealed; rows of other
        // owners or unsealed rows stay in the store.
        let events = self
            .inner
            .session_transcript_rows(principal, session_key, limit)
            .await?;
        Ok(events
            .into_iter()
            .filter(|event| self.owns_event(event))
            .map(|event| self.reveal(event))
            .collect())
    }

    /// Scheduled-run results are the one read that returns plaintext rows.
    /// Scheduled runs record outside any encrypted scope (`scheduled_result`
    /// writes through the run's base memory), so their rows are plaintext rows
    /// of the owner. Only those scheduled-result types are read; any other
    /// event type stays empty. Inner filters workspace, owner, event type and
    /// partition in SQL, so every row here belongs to the principal's owner.
    /// Sealed rows go through `owns_event` and `reveal`; sealed rows of another
    /// scope are dropped; plaintext rows are returned as stored.
    async fn newest_owner_events_of_type(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        event_type: &str,
        after_id: Option<i64>,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
        if !SCHEDULED_RESULT_EVENT_TYPES.contains(&event_type) {
            return Ok(Vec::new());
        }
        let events = self
            .inner
            .newest_owner_events_of_type(principal, event_type, after_id, limit)
            .await?;
        Ok(events
            .into_iter()
            .filter_map(|event| {
                if self.owns_event(&event) {
                    Some(self.reveal(event))
                } else if self.looks_sealed(&event.content) {
                    None
                } else {
                    Some(event)
                }
            })
            .collect())
    }

    async fn excluded_transcript_rows(&self, row_ids: &[i64]) -> anyhow::Result<std::collections::HashSet<i64>> {
        self.inner.excluded_transcript_rows(row_ids).await
    }

    async fn message_event_ids_in_session(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        event_ids: &[String],
    ) -> anyhow::Result<std::collections::HashSet<String>> {
        self.inner.message_event_ids_in_session(principal, event_ids).await
    }

    async fn search_transcripts(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        session_key: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::traits::TranscriptHit>> {
        self.search_sealed_transcripts(principal, query, session_key, limit)
            .await
    }

    async fn load_session_context_before(
        &self,
        query: crate::memory::SessionContextQuery,
        before_id: i64,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
        let events = self.inner.load_session_context_before(query, before_id).await?;
        // A page is complete only on what the store holds: dropping a row this
        // scope cannot open would make a short page look like the session start.
        let unreadable_rows: usize = events.iter().filter(|event| !self.owns_event(event)).count();
        if unreadable_rows > 0 {
            tracing::warn!(
                unreadable_rows,
                "session context page holds rows this scope cannot decrypt; it is not provably complete"
            );
            return Err(crate::memory::traits::SessionPageIncomplete { unreadable_rows }.into());
        }
        Ok(events.into_iter().map(|event| self.reveal(event)).collect())
    }

    async fn append_memory_event(
        &self,
        mut input: crate::memory::MemoryEventInput,
    ) -> anyhow::Result<crate::memory::MemoryEvent> {
        let id = input
            .event_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        input.event_id = Some(id.clone());
        let sealed = self.seal(&format!("memory-event:{id}"), &serde_json::to_string(&input)?)?;
        input.subject_id = self.stored_key(&input.subject_id);
        input.payload_json = Some(sealed);
        self.open_memory_event(self.inner.append_memory_event(input).await?)
            .ok_or_else(|| anyhow::anyhow!("scoped memory event cannot be opened"))
    }

    async fn list_memory_events_since(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEvent>> {
        Ok(self
            .inner
            .list_memory_events_since(principal, after_id, limit)
            .await?
            .into_iter()
            .filter_map(|event| self.open_memory_event(event))
            .collect())
    }

    async fn list_memory_events_recent(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::MemoryEvent>> {
        Ok(self
            .inner
            .list_memory_events_recent(principal, limit)
            .await?
            .into_iter()
            .filter_map(|event| self.open_memory_event(event))
            .collect())
    }

    async fn create_memory_draft(
        &self,
        mut input: crate::memory::MemoryDraftInput,
    ) -> anyhow::Result<crate::memory::MemoryDraft> {
        let id = input
            .draft_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let stored_id = self.stored_key(&format!("draft:{id}"));
        input.draft_id = Some(stored_id.clone());
        let payload =
            serde_json::json!({"draft_id": id, "session_key": input.session_key, "payload_json": input.payload_json});
        input.session_key = Some(
            self.crypto
                .mac(&self.account_id, "memory-session", self.scope.as_bytes())?,
        );
        input.payload_json = Some(self.seal(&format!("draft-meta:{stored_id}"), &payload.to_string())?);
        input.content = self.seal(&input.key, &input.content)?;
        input.key = self.stored_key(&input.key);
        self.open_draft(self.inner.create_memory_draft(input).await?)
            .ok_or_else(|| anyhow::anyhow!("scoped draft cannot be opened"))
    }

    async fn list_memory_drafts_for_run(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        worker_run_id: &str,
    ) -> anyhow::Result<Vec<crate::memory::MemoryDraft>> {
        Ok(self
            .inner
            .list_memory_drafts_for_run(principal, worker_run_id)
            .await?
            .into_iter()
            .filter_map(|draft| self.open_draft(draft))
            .collect())
    }

    async fn merge_memory_draft(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        draft_id: &str,
    ) -> anyhow::Result<Option<crate::memory::MemoryDraft>> {
        let stored_id = self.stored_key(&format!("draft:{draft_id}"));
        Ok(self
            .inner
            .merge_memory_draft(principal, &stored_id)
            .await?
            .and_then(|draft| self.open_draft(draft)))
    }

    async fn reject_memory_draft(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        draft_id: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<Option<crate::memory::MemoryDraft>> {
        let stored_id = self.stored_key(&format!("draft:{draft_id}"));
        let sealed_reason = reason
            .map(|reason| self.seal(&format!("draft-reject:{draft_id}"), reason))
            .transpose()?;
        Ok(self
            .inner
            .reject_memory_draft(principal, &stored_id, sealed_reason.as_deref())
            .await?
            .and_then(|draft| self.open_draft(draft)))
    }
}

/// Factory used for one ramflux account and conversation scope.
#[must_use]
pub fn create_ramflux_scoped_memory(
    inner: Arc<dyn Memory>,
    crypto: Arc<AccountCrypto>,
    account_id: impl Into<String>,
    scope: impl Into<String>,
) -> Arc<dyn Memory> {
    Arc::new(EncryptedScope::new(inner, crypto, account_id, scope))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic)]
mod tests;
