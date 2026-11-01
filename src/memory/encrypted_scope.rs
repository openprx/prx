//! Memory decorator for one ramflux conversation scope.
//!
//! Content is sealed with the account DEK before it reaches the inner backend.
//! Every read filters to this scope and opens the ciphertext before matching.
//! Bytes that were not sealed for this scope are not returned. The sqlite and
//! postgres backends themselves are unchanged.

use crate::memory::traits::{ConversationTurn, Memory, MemoryCategory, MemoryEntry};
use crate::security::ramflux_aead::{AccountCrypto, CryptoError, id_hash};
use async_trait::async_trait;
use base64::Engine;
use std::collections::HashMap;
use std::sync::Arc;

/// How many namespace rows `recall` will open before it stops.
const RECALL_SCAN_LIMIT: usize = 200;

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
        format!("rf:{}:", id_hash(&self.scope))
    }

    fn namespaced(&self, key: &str) -> String {
        format!("{}{key}", self.prefix())
    }

    fn seal(&self, record_id: &str, content: &str) -> Result<String, CryptoError> {
        let blob = self
            .crypto
            .seal(&self.account_id, &self.scope, record_id, content.as_bytes())?;
        if self.crypto.encryption_enabled() {
            Ok(base64::engine::general_purpose::STANDARD.encode(blob))
        } else {
            Ok(String::from_utf8(blob).unwrap_or_default())
        }
    }

    fn open_record(&self, record_id: &str, stored: &str) -> Option<String> {
        if !self.crypto.encryption_enabled() {
            return Some(stored.to_string());
        }
        let blob = base64::engine::general_purpose::STANDARD
            .decode(stored.as_bytes())
            .ok()?;
        if !blob.starts_with(b"RF1") {
            return None;
        }
        let plain = self.crypto.open(&self.account_id, &self.scope, record_id, &blob).ok()?;
        String::from_utf8(plain).ok()
    }

    fn original_key<'a>(&self, stored_key: &'a str) -> Option<&'a str> {
        stored_key.strip_prefix(&self.prefix())
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
        event
    }
}

#[async_trait]
impl Memory for EncryptedScope {
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
        self.inner
            .store(&self.namespaced(key), &sealed, category, Some(self.scope.as_str()))
            .await
    }

    async fn recall(&self, query: &str, limit: usize, _session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        let entries = self.inner.list(None, None).await?;
        let needle = query.to_lowercase();
        let mut matched = Vec::new();
        let mut scanned = 0usize;
        for mut entry in entries {
            let Some(record_id) = self.original_key(&entry.key).map(str::to_string) else {
                continue;
            };
            if scanned >= RECALL_SCAN_LIMIT {
                break;
            }
            scanned = scanned.saturating_add(1);
            let Some(plain) = self.open_record(&record_id, &entry.content) else {
                continue;
            };
            if !needle.is_empty() && !plain.to_lowercase().contains(&needle) {
                continue;
            }
            entry.content = plain;
            entry.key = record_id;
            matched.push(entry);
            if matched.len() >= limit {
                break;
            }
        }
        Ok(matched)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let Some(mut entry) = self.inner.get(&self.namespaced(key)).await? else {
            return Ok(None);
        };
        let Some(plain) = self.open_record(key, &entry.content) else {
            return Ok(None);
        };
        entry.content = plain;
        entry.key = key.to_string();
        Ok(Some(entry))
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let entries = self.inner.list(category, None).await?;
        let mut own = Vec::new();
        for mut entry in entries {
            let Some(record_id) = self.original_key(&entry.key).map(str::to_string) else {
                continue;
            };
            let Some(plain) = self.open_record(&record_id, &entry.content) else {
                continue;
            };
            entry.content = plain;
            entry.key = record_id;
            own.push(entry);
        }
        Ok(own)
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.inner.forget(&self.namespaced(key)).await
    }

    async fn count(&self) -> anyhow::Result<usize> {
        Ok(self.list(None, None).await?.len())
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
        let record_id = input.idempotency_key.clone().unwrap_or_else(|| "event".to_string());
        input.content = self.seal(&record_id, &input.content)?;
        if let Some(payload) = input.raw_payload_json.as_deref() {
            input.raw_payload_json = Some(self.seal(&format!("{record_id}:payload"), payload)?);
        }
        let event = self.inner.append_message_event(input).await?;
        Ok(self.reveal(event))
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
        Ok(event.map(|event| self.reveal(event)))
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
        Ok(event.map(|event| self.reveal(event)))
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
        Ok(event.map(|event| self.reveal(event)))
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
        let record = message_id.unwrap_or(session_key);
        let key = format!("convturn:{session_key}:{record}");
        let payload = serde_json::json!({
            "role": role,
            "content": content,
            "session": session_key,
            "message_id": message_id,
            "timestamp": timestamp,
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
            if turns.len() >= max_turns_per_session {
                continue;
            }
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
        Ok(out)
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
}
