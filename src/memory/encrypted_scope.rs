//! Memory decorator for one ramflux conversation scope.
//!
//! Content is sealed with the account DEK before it reaches the inner backend.
//! Every read filters to this scope and opens the ciphertext before matching.
//! Bytes that were not sealed for this scope are not returned. Transactional
//! erasure requires a backend that implements the ownership-index contract.

use crate::memory::RAMFLUX_MEMORY_KEY_PREFIX;
use crate::memory::traits::{ConversationTurn, Memory, MemoryCategory, MemoryEntry};
use crate::security::ramflux_aead::{AccountCrypto, CryptoError, id_hash};
use async_trait::async_trait;
use base64::Engine;
use std::collections::HashMap;
use std::sync::Arc;

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
        let aad = stored_key.rsplit(':').next()?;
        let plain = self.open_aad(aad, stored)?;
        let value: serde_json::Value = serde_json::from_str(&plain).ok()?;
        if !stored_key.starts_with(&self.prefix())
            || value
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

    fn name(&self) -> &str {
        // The ledger and event recorder select a backend by this name. The
        // inner store is that backend; this view only seals the bytes it writes.
        self.inner.name()
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let sealed = self.seal(key, content)?;
        let session = self
            .crypto
            .mac(&self.account_id, "memory-session", self.scope.as_bytes())?;
        let owner = self
            .crypto
            .mac(&self.account_id, "memory-owner", self.account_id.as_bytes())?;
        // Sealed payloads carry attribution without entering plaintext topic
        // classification, which would create a copy outside message erasure.
        let metadata = crate::memory::MemoryStoreMetadata {
            channel: Some("ramflux".to_owned()),
            owner_id: Some(owner),
            ..crate::memory::MemoryStoreMetadata::default()
        };
        self.inner
            .store_with_metadata(&self.stored_key(key), &sealed, category, Some(&session), metadata)
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

    fn persisted_key(&self, key: &str) -> String {
        self.stored_key(key)
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.inner.forget(&self.stored_key(key)).await
    }

    async fn forget_message_idempotency(&self, idempotency_key: &str) -> anyhow::Result<usize> {
        self.inner.forget_message_idempotency(idempotency_key).await
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
        if let Some(payload) = input.raw_payload_json.clone() {
            let record_id = input.idempotency_key.clone().unwrap_or_else(|| "event".to_string());
            input.raw_payload_json = Some(self.seal(&format!("{record_id}:payload"), &payload)?);
        }
        let event = self.inner.append_message_event(input).await?;
        Ok(self.reveal(event))
    }

    async fn append_message_event_until(
        &self,
        mut input: crate::memory::MessageEventInput,
        deadline: std::time::Instant,
    ) -> anyhow::Result<crate::memory::MessageEvent> {
        self.seal_event_content(&mut input)?;
        if let Some(payload) = input.raw_payload_json.take() {
            let key = input.idempotency_key.as_deref().unwrap_or("event");
            input.raw_payload_json = Some(self.seal(&format!("{key}:payload"), &payload)?);
        }
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
            event.raw_payload_json = Some(self.seal(&format!("{key}:payload"), &payload)?);
        }
        self.inner.project_console_message_until(&event, deadline).await
    }

    async fn list_message_events_since(
        &self,
        principal: &crate::memory::MemoryPrincipal,
        after_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::memory::MessageEvent>> {
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
    ) -> anyhow::Result<crate::memory::ingress_recovery::IngressAppendOutcome> {
        let mut input = input;
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
            .append_ingress_input(input, capsule, lifetime, deadline)
            .await?;
        Ok(match outcome {
            crate::memory::ingress_recovery::IngressAppendOutcome::Inserted(event) => {
                crate::memory::ingress_recovery::IngressAppendOutcome::Inserted(self.reveal(event))
            }
            crate::memory::ingress_recovery::IngressAppendOutcome::AlreadyAccepted(event) => {
                crate::memory::ingress_recovery::IngressAppendOutcome::AlreadyAccepted(self.reveal(event))
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
mod tests {
    use super::*;
    use parking_lot::Mutex;

    struct MapMemory {
        rows: Mutex<HashMap<String, MemoryEntry>>,
    }

    impl MapMemory {
        fn new() -> Self {
            Self {
                rows: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl Memory for MapMemory {
        fn name(&self) -> &str {
            "map"
        }

        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.rows.lock().insert(
                key.to_string(),
                MemoryEntry {
                    origin: crate::memory::traits::MemoryEntryOrigin::Stored,
                    source_event_id: None,
                    id: key.to_string(),
                    key: key.to_string(),
                    content: content.to_string(),
                    category,
                    timestamp: "t".to_string(),
                    session_id: session_id.map(str::to_string),
                    score: None,
                    tags: None,
                    access_count: None,
                    useful_count: None,
                    source: None,
                    source_confidence: None,
                    verification_status: None,
                    lifecycle_state: None,
                    compressed_from: None,
                },
            );
            Ok(())
        }

        async fn recall(
            &self,
            _query: &str,
            _limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(self.rows.lock().values().cloned().collect())
        }

        async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(self.rows.lock().get(key).cloned())
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(self.rows.lock().values().cloned().collect())
        }

        async fn forget(&self, key: &str) -> anyhow::Result<bool> {
            Ok(self.rows.lock().remove(key).is_some())
        }

        async fn count(&self) -> anyhow::Result<usize> {
            Ok(self.rows.lock().len())
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    fn scope(inner: Arc<dyn Memory>, account: &str, conversation: &str) -> Arc<dyn Memory> {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("test: temp")));
        let crypto = Arc::new(AccountCrypto::new(dir.path(), dir.path(), true));
        crypto.unlock(account).expect("test: unlock");
        let namespace = format!("{account}/{conversation}");
        create_ramflux_scoped_memory(inner, crypto, account, namespace)
    }

    #[tokio::test]
    async fn two_accounts_and_two_conversations_stay_apart() {
        let inner: Arc<dyn Memory> = Arc::new(MapMemory::new());
        let a = scope(Arc::clone(&inner), "acct-a", "conv-1");
        let b = scope(Arc::clone(&inner), "acct-a", "conv-2");
        let c = scope(Arc::clone(&inner), "acct-b", "conv-1");
        a.store("note", "alpha secret", MemoryCategory::Conversation, None)
            .await
            .expect("test: store a");
        b.store("note", "beta secret", MemoryCategory::Conversation, None)
            .await
            .expect("test: store b");
        let got = a.get("note").await.expect("test: get").expect("test: row");
        assert_eq!(got.content, "alpha secret");
        assert!(b.get("note").await.expect("test: get b").is_some());
        assert_ne!(
            b.get("note").await.expect("test: get b2").expect("test: row").content,
            "alpha secret"
        );
        assert!(c.get("note").await.expect("test: get c").is_none());
        let recalled = a.recall("alpha", 5, None).await.expect("test: recall");
        assert_eq!(recalled.len(), 1);
        assert!(a.recall("beta", 5, None).await.expect("test: recall other").is_empty());
        assert_eq!(a.count().await.expect("test: count"), 1);
        assert_eq!(c.list(None, None).await.expect("test: list").len(), 0);
        let dumped = inner.list(None, None).await.expect("test: dump");
        for row in dumped {
            assert!(!row.content.contains("alpha secret"));
            assert!(!row.content.contains("beta secret"));
        }
    }

    #[tokio::test]
    async fn history_round_trip_decrypts_and_plaintext_is_not_passed_through() {
        let inner: Arc<dyn Memory> = Arc::new(MapMemory::new());
        inner
            .store("foreign", "plain neighbor", MemoryCategory::Core, None)
            .await
            .expect("test: foreign");
        let scoped = scope(Arc::clone(&inner), "acct-a", "conv-1");
        scoped
            .append_conversation_turn(
                "sess",
                "ramflux",
                "alice",
                "user",
                "hello scope",
                None,
                Some("m1"),
                None,
            )
            .await
            .expect("test: append");
        let principal = crate::memory::MemoryPrincipal {
            workspace_id: "ws".into(),
            ..crate::memory::MemoryPrincipal::default()
        };
        let loaded = scoped
            .load_recent_conversation_histories(&principal, 10, 10)
            .await
            .expect("test: load");
        let turns = loaded.get("sess").expect("test: session");
        assert_eq!(turns.len(), 1);
        assert_eq!(turns.first().expect("test: turn").content, "hello scope");
        assert!(scoped.get("foreign").await.expect("test: foreign get").is_none());
    }
    #[tokio::test]
    async fn logical_prefix_forget_stays_in_its_scope() {
        let inner: Arc<dyn Memory> = Arc::new(MapMemory::new());
        let a = scope(Arc::clone(&inner), "acct", "conv-a");
        let b = scope(Arc::clone(&inner), "acct", "conv-b");
        a.store("topic/a", "remove", MemoryCategory::Core, None)
            .await
            .expect("test: own");
        a.store("other", "own keep", MemoryCategory::Core, None)
            .await
            .expect("test: own keep");
        b.store("topic/b", "neighbor keep", MemoryCategory::Core, None)
            .await
            .expect("test: neighbor");
        inner
            .store("topic/foreign", "plain foreign", MemoryCategory::Core, None)
            .await
            .expect("test: foreign");
        assert_eq!(a.list_key_prefix("topic/", 10).await.expect("test: list").len(), 1);
        assert_eq!(a.forget_key_prefix("topic/").await.expect("test: forget"), 1);
        assert!(a.get("topic/a").await.expect("test: deleted").is_none());
        assert!(a.get("other").await.expect("test: own keep").is_some());
        assert!(b.get("topic/b").await.expect("test: neighbor keep").is_some());
        assert!(inner.get("topic/foreign").await.expect("test: foreign keep").is_some());
    }

    #[tokio::test]
    async fn history_keeps_newest_timestamped_message_identities() {
        let inner: Arc<dyn Memory> = Arc::new(MapMemory::new());
        let view = scope(inner, "acct", "conv");
        for (id, time) in [
            ("m1", "2026-11-01T01:00:00Z"),
            ("m3", "2026-11-01T03:00:00Z"),
            ("m2", "2026-11-01T02:00:00Z"),
        ] {
            view.append_conversation_turn(
                "session",
                "ramflux",
                "sender",
                "assistant",
                id,
                Some(time),
                Some(id),
                None,
            )
            .await
            .expect("test: append");
        }
        let principal = crate::memory::MemoryPrincipal {
            workspace_id: "ws".to_owned(),
            agent_id: None,
            persona_id: None,
            session_key: None,
            channel: None,
            sender: None,
            owner_id: None,
            require_owner_match: false,
            legacy_session_key: None,
        };
        let rows = view
            .load_recent_conversation_histories(&principal, 2, 10)
            .await
            .expect("test: history");
        let turns = rows.get("session").expect("test: session");
        assert_eq!(
            turns
                .iter()
                .filter_map(|turn| turn.message_id.as_deref())
                .collect::<Vec<_>>(),
            vec!["m2", "m3"]
        );
        assert!(turns.iter().all(|turn| !turn.timestamp.is_empty()));
    }
}
