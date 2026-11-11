//! ChildSession ownership object (TUI-1a). Not wired to keys. Mailbox-cursor
//! reconstruction is MSG-CORE.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::approval::PendingApprovals;
use crate::messaging::{ExecRef, InheritedProvenance, SessionAddr};
use crate::tools::sessions_spawn::HistoryEntry;

/// Inbound mailbox consume cursor. MSG-CORE fills reconstruction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailboxCursor {}

/// Provider binding without credentials (no key wiring).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderBinding {
    /// Provider name.
    pub provider: String,
    /// Model id.
    pub model: String,
}

/// Process-mode relaunch recipe: same [`SessionAddr`], new run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelaunchRecipe {
    /// Stable address across relaunch.
    pub addr: SessionAddr,
    /// Last completed run, if any.
    pub last_run: Option<String>,
    /// Sealed provenance restored on relaunch.
    pub inherited_provenance: Option<InheritedProvenance>,
}

/// One child session owned by the parent chat for its lifetime.
pub struct ChildSession {
    addr: SessionAddr,
    history: Vec<HistoryEntry>,
    provider: Option<ProviderBinding>,
    mailbox_cursor: MailboxCursor,
    approvals: PendingApprovals,
    turn: Mutex<()>,
    recipe: Option<RelaunchRecipe>,
}

impl ChildSession {
    /// Construct a live child session.
    #[must_use]
    pub fn new(addr: SessionAddr, provider: Option<ProviderBinding>, recipe: Option<RelaunchRecipe>) -> Self {
        Self {
            addr: addr.clone(),
            history: Vec::new(),
            provider,
            mailbox_cursor: MailboxCursor::default(),
            approvals: PendingApprovals::new(),
            turn: Mutex::new(()),
            recipe: recipe.or(Some(RelaunchRecipe {
                addr,
                last_run: None,
                inherited_provenance: None,
            })),
        }
    }

    /// Logical address. No run field.
    #[must_use]
    pub const fn addr(&self) -> &SessionAddr {
        &self.addr
    }

    /// This session's approval router.
    #[must_use]
    pub const fn approvals(&self) -> &PendingApprovals {
        &self.approvals
    }

    /// History reconstructed from the registry.
    #[must_use]
    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    /// Provider binding, if any.
    #[must_use]
    pub const fn provider(&self) -> Option<&ProviderBinding> {
        self.provider.as_ref()
    }

    /// Mailbox cursor (empty until MSG-CORE).
    #[must_use]
    pub const fn mailbox_cursor(&self) -> &MailboxCursor {
        &self.mailbox_cursor
    }

    /// Acquire the turn mutex.
    pub fn lock_turn(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.turn.lock()
    }

    /// Process relaunch recipe, if this is a process child.
    #[must_use]
    pub const fn recipe(&self) -> Option<&RelaunchRecipe> {
        self.recipe.as_ref()
    }

    fn metadata(&self) -> ChildSessionMetadata {
        ChildSessionMetadata {
            addr: self.addr.clone(),
            provider: self.provider.clone(),
            recipe: self.recipe.clone(),
        }
    }
}

/// Child identity and relaunch metadata stored in the dedicated child-session record.
/// History uses the record's normal turns; approvals and live handles are never saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildSessionMetadata {
    pub addr: SessionAddr,
    pub provider: Option<ProviderBinding>,
    pub recipe: Option<RelaunchRecipe>,
}

/// Refuse a caller manifest whose identity differs from the logical child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelaunchIdentityError {
    Owner,
    Agent,
}

impl std::fmt::Display for RelaunchIdentityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Owner => "relaunch manifest owner differs from session address",
            Self::Agent => "relaunch manifest agent differs from session address",
        })
    }
}
impl std::error::Error for RelaunchIdentityError {}

/// Durable child registry, isolated from the operator saved-chat namespace.
#[derive(Clone)]
pub struct ChildSessionRegistry {
    memory: Arc<dyn crate::memory::Memory>,
}

impl ChildSessionRegistry {
    /// Use the same durable backend as the parent session registry.
    pub fn new(memory: Arc<dyn crate::memory::Memory>) -> Self {
        Self { memory }
    }

    fn record_id(addr: &SessionAddr) -> Result<String> {
        Ok(format!(
            "child-{}",
            hex::encode(Sha256::digest(serde_json::to_vec(addr)?))
        ))
    }

    /// Mutate the complete stored record under the backend's transaction.
    async fn update_record<F>(&self, addr: &SessionAddr, mut update: F) -> Result<crate::chat::session::ChatSession>
    where
        F: FnMut(Option<crate::chat::session::ChatSession>) -> Result<crate::chat::session::ChatSession>
            + Send
            + 'static,
    {
        let key = Self::record_id(addr)?;
        let addr = addr.clone();
        let memory = Arc::clone(&self.memory);
        crate::runtime::blocking::spawn_blocking(move || -> Result<_> {
            let store = memory
                .child_session_store()
                .context("transactional child session store unsupported")?;
            let json = store.update(&key, &mut |old| {
                let old = old.map(crate::chat::session::ChatSession::from_json).transpose()?;
                if let Some(record) = &old {
                    Self::validate_record(record, &addr, &key)?;
                }
                let record = update(old)?;
                Self::validate_record(&record, &addr, &key)?;
                Ok(record.to_json()?)
            })?;
            Ok(crate::chat::session::ChatSession::from_json(&json)?)
        })
        .await?
    }

    fn validate_record(record: &crate::chat::session::ChatSession, addr: &SessionAddr, key: &str) -> Result<()> {
        anyhow::ensure!(
            record.id == key
                && record
                    .child_session
                    .as_ref()
                    .is_some_and(|metadata| metadata.addr == *addr),
            "child session registry address mismatch"
        );
        Ok(())
    }

    /// Create a record or replace only metadata/history, retaining the record's
    /// title, creation time, usage and summaries. Relaunch owns `last_run`.
    pub async fn insert(&self, child: &ChildSession) -> Result<()> {
        let metadata = child.metadata();
        let history = child.history.clone();
        let key = Self::record_id(child.addr())?;
        self.update_record(child.addr(), move |old| {
            let mut metadata = metadata.clone();
            let mut record = old.unwrap_or_else(|| {
                let mut record = crate::chat::session::ChatSession::new(
                    metadata
                        .provider
                        .as_ref()
                        .map_or("", |provider| provider.provider.as_str()),
                    metadata
                        .provider
                        .as_ref()
                        .map_or("", |provider| provider.model.as_str()),
                );
                record.id = key.clone();
                record
            });
            if let Some(recipe) = metadata.recipe.as_mut()
                && let Some(previous) = record.child_session.as_ref().and_then(|value| value.recipe.as_ref())
            {
                recipe.last_run.clone_from(&previous.last_run);
            }
            record.child_session = Some(metadata);
            record.turns = history
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let content = crate::chat::sanitize::sanitize_for_persistence(&entry.content);
                    let tool_calls = record
                        .turns
                        .get(index)
                        .filter(|turn| {
                            turn.role == entry.role && turn.content == content && turn.timestamp == entry.timestamp
                        })
                        .map_or_else(Vec::new, |turn| turn.tool_calls.clone());
                    crate::chat::session::ChatTurn {
                        role: entry.role.clone(),
                        content,
                        timestamp: entry.timestamp,
                        tool_calls,
                    }
                })
                .collect();
            record.updated_at = chrono::Utc::now();
            Ok(record)
        })
        .await?;
        Ok(())
    }

    async fn load_record(&self, addr: &SessionAddr) -> Result<crate::chat::session::ChatSession> {
        let key = Self::record_id(addr)?;
        let memory = Arc::clone(&self.memory);
        let json = crate::runtime::blocking::spawn_blocking(move || {
            memory
                .child_session_store()
                .context("transactional child session store unsupported")?
                .load(&key)?
                .context("child session not in registry")
        })
        .await??;
        let record = crate::chat::session::ChatSession::from_json(&json)?;
        Self::validate_record(&record, addr, &Self::record_id(addr)?)?;
        Ok(record)
    }

    /// Reopen the durable registry record only. Ask dies; no worker is revived.
    pub async fn reconstruct(&self, addr: &SessionAddr) -> Result<ChildSession> {
        let record = self.load_record(addr).await?;
        let metadata = record
            .child_session
            .ok_or_else(|| anyhow::anyhow!("session has no child identity"))?;
        if metadata.addr != *addr {
            bail!("child session registry address mismatch");
        }
        Ok(ChildSession {
            addr: metadata.addr,
            history: record
                .turns
                .into_iter()
                .map(|turn| HistoryEntry {
                    role: turn.role,
                    content: turn.content,
                    timestamp: turn.timestamp,
                })
                .collect(),
            provider: metadata.provider,
            recipe: metadata.recipe,
            mailbox_cursor: MailboxCursor::default(),
            approvals: PendingApprovals::new(),
            turn: Mutex::new(()),
        })
    }

    async fn relaunch_record(&self, addr: &SessionAddr) -> Result<crate::chat::session::ChatSession> {
        let expected = addr.clone();
        self.update_record(addr, move |old| {
            let mut record = old.context("child session not in registry")?;
            let recipe = record
                .child_session
                .as_mut()
                .and_then(|metadata| metadata.recipe.as_mut())
                .context("child session has no relaunch recipe")?;
            anyhow::ensure!(recipe.addr == expected, "relaunch recipe address mismatch");
            recipe.last_run = Some(Uuid::now_v7().to_string());
            record.updated_at = chrono::Utc::now();
            Ok(record)
        })
        .await
    }

    /// Change only the run identity and update time in one store transaction.
    pub async fn relaunch(&self, addr: &SessionAddr) -> Result<ExecRef> {
        let record = self.relaunch_record(addr).await?;
        Ok(ExecRef {
            run: record
                .child_session
                .and_then(|metadata| metadata.recipe)
                .and_then(|recipe| recipe.last_run),
        })
    }

    /// Build the relaunch manifest from registry provenance. The caller seals
    /// the complete new manifest HMAC after supplying fresh worker authority.
    pub async fn relaunch_worker(
        &self,
        addr: &SessionAddr,
        mut manifest: crate::session_worker::protocol::WorkerManifest,
    ) -> Result<crate::session_worker::protocol::WorkerManifest> {
        if manifest.owner_id.as_deref() != Some(addr.owner.as_str()) {
            return Err(RelaunchIdentityError::Owner.into());
        }
        if manifest.agent_id.as_deref().unwrap_or("main") != addr.agent {
            return Err(RelaunchIdentityError::Agent.into());
        }
        let record = self.relaunch_record(addr).await?;
        let recipe = record
            .child_session
            .and_then(|metadata| metadata.recipe)
            .context("relaunch recipe missing")?;
        manifest.run_id = recipe.last_run.context("relaunch returned no run")?;
        // The durable key frames instance, owner, session and agent together.
        manifest.session_scope_key = Self::record_id(addr)?;
        manifest.session_addr = Some(recipe.addr);
        manifest.inherited_provenance = recipe.inherited_provenance;
        manifest.parent_capability = None;
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(session: &str) -> SessionAddr {
        SessionAddr::new("prx-default", "owner-a", session, Some("main"))
    }

    #[test]
    fn two_child_sessions_hold_independent_approvals() {
        let a = ChildSession::new(addr("child-a"), None, None);
        let b = ChildSession::new(addr("child-b"), None, None);
        a.approvals().test_register("run-a");
        assert!(a.approvals().is_pending("run-a"));
        assert!(!b.approvals().is_pending("run-a"));
        assert!(!a.approvals().is_pending("run-b"));
        b.approvals().test_register("run-b");
        assert!(b.approvals().is_pending("run-b"));
        assert!(!a.approvals().is_pending("run-b"));
    }

    #[tokio::test]
    async fn child_session_reconstructs_from_registry() {
        let dir = tempfile::TempDir::new().expect("registry directory");
        let registry = ChildSessionRegistry::new(Arc::new(
            crate::memory::SqliteMemory::new(dir.path()).expect("registry backend"),
        ));
        let mut live = ChildSession::new(
            addr("child-a"),
            Some(ProviderBinding {
                provider: "openrouter".into(),
                model: "m".into(),
            }),
            None,
        );
        live.history.push(HistoryEntry {
            role: "user".into(),
            content: "hello".into(),
            timestamp: chrono::Utc::now(),
        });
        live.approvals().test_register("ask-1");
        registry.insert(&live).await.expect("persist child");
        drop(live);
        drop(registry);
        let registry = ChildSessionRegistry::new(Arc::new(
            crate::memory::SqliteMemory::new(dir.path()).expect("reopened registry backend"),
        ));
        let restored = registry.reconstruct(&addr("child-a")).await.expect("test: reconstruct");
        assert_eq!(restored.addr(), &addr("child-a"));
        assert_eq!(restored.history().len(), 1);
        assert_eq!(restored.provider().map(|p| p.provider.as_str()), Some("openrouter"));
        assert!(!restored.approvals().is_pending("ask-1"), "in-memory Ask dies");
        assert_eq!(restored.mailbox_cursor(), &MailboxCursor::default());
    }

    #[tokio::test]
    async fn relaunch_keeps_session_addr_and_mints_new_run() {
        let dir = tempfile::TempDir::new().expect("registry directory");
        let registry = ChildSessionRegistry::new(Arc::new(
            crate::memory::SqliteMemory::new(dir.path()).expect("registry backend"),
        ));
        let session = ChildSession::new(addr("child-a"), None, None);
        registry.insert(&session).await.expect("persist child");
        let first = registry.relaunch(&addr("child-a")).await.expect("test: first");
        let second = registry.relaunch(&addr("child-a")).await.expect("test: second");
        assert_ne!(first.run, second.run);
        assert!(first.run.is_some());
        let restored = registry.reconstruct(&addr("child-a")).await.expect("test: reconstruct");
        assert_eq!(restored.addr(), &addr("child-a"));
        assert_eq!(restored.recipe().and_then(|r| r.last_run.clone()), second.run);
    }

    #[tokio::test]
    async fn child_registry_is_excluded_from_picker_resume_and_latest() {
        let dir = tempfile::tempdir().expect("session database");
        let memory: Arc<dyn crate::memory::Memory> =
            Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("sqlite"));
        let registry = ChildSessionRegistry::new(Arc::clone(&memory));
        let child = ChildSession::new(addr("child-a"), None, None);
        registry.insert(&child).await.expect("insert child");
        registry.relaunch(child.addr()).await.expect("relaunch child");
        assert!(
            memory
                .list(Some(&crate::memory::MemoryCategory::Conversation), None)
                .await
                .expect("operator namespace")
                .is_empty(),
            "child records must never be written into operator memories"
        );
        assert!(
            crate::chat::session_store::saved_chat_sessions(memory.as_ref())
                .await
                .expect("picker")
                .is_empty()
        );
        assert!(
            crate::chat::session_store::load_latest_session(memory.as_ref())
                .await
                .expect("latest")
                .is_none()
        );
        assert!(
            crate::chat::session_store::load_session_by_id(
                memory.as_ref(),
                &ChildSessionRegistry::record_id(child.addr()).expect("id")
            )
            .await
            .expect("resume")
            .is_none()
        );
        let mut operator = crate::chat::session::ChatSession::new("prx_provider", "prx_model");
        operator.updated_at = chrono::DateTime::from_timestamp(1, 0).expect("old time");
        crate::chat::session_store::save_session(memory.as_ref(), &operator)
            .await
            .expect("save operator");
        // Also hide marked blobs written by the earlier, never-activated API.
        let mut legacy = registry.load_record(child.addr()).await.expect("child record");
        legacy.id = "prx-legacy-child".into();
        crate::chat::session_store::save_session(memory.as_ref(), &legacy)
            .await
            .expect("legacy blob");
        registry.relaunch(child.addr()).await.expect("newest child activity");
        let picker = crate::chat::session_store::saved_chat_sessions(memory.as_ref())
            .await
            .expect("picker");
        assert_eq!(picker.len(), 1);
        assert_eq!(picker.first().expect("operator row").id, operator.id);
        assert_eq!(
            crate::chat::session_store::load_latest_session(memory.as_ref())
                .await
                .expect("latest")
                .expect("operator")
                .id,
            operator.id
        );
        assert!(
            crate::chat::session_store::load_session_by_id(memory.as_ref(), &legacy.id)
                .await
                .expect("legacy direct resume")
                .is_none()
        );
        registry
            .reconstruct(child.addr())
            .await
            .expect("child still reconstructs");
    }

    async fn rich_child(registry: &ChildSessionRegistry) -> ChildSession {
        let mut child = ChildSession::new(addr("child-a"), None, None);
        child.history.push(HistoryEntry {
            role: "assistant".into(),
            content: "prx result".into(),
            timestamp: chrono::Utc::now(),
        });
        registry.insert(&child).await.expect("insert child");
        registry
            .update_record(child.addr(), |old| {
                let mut record = old.expect("stored child");
                record.title = "prx preserved title".into();
                record.created_at = chrono::DateTime::from_timestamp(1, 0).expect("creation time");
                record
                    .token_usage_records
                    .push(crate::llm::route_decision::MeteredTokenUsageRecord {
                        settlement_id: Some("prx-settlement".into()),
                        provider: "prx_provider".into(),
                        model: "prx_model".into(),
                        prompt_tokens: 2,
                        completion_tokens: 3,
                        total_tokens: 5,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                        source: crate::llm::route_decision::TokenUsageSource::Reported,
                        cost_usd: Some(0.25),
                    });
                record
                    .background_sessions
                    .push(crate::chat::sessions::PersistedSessionSummary {
                        id: "prx-summary".into(),
                        seq: 1,
                        kind: "agent".into(),
                        origin: "model".into(),
                        status: "completed".into(),
                        title: "prx task".into(),
                        summary: "prx summary".into(),
                        token_usage_records: Vec::new(),
                        created_at: record.created_at,
                    });
                record
                    .turns
                    .first_mut()
                    .expect("turn")
                    .tool_calls
                    .push(crate::chat::session::ToolCallSummary {
                        name: "prx_probe".into(),
                        args_preview: "{}".into(),
                        success: true,
                        task_id: Some(1),
                        sequence: Some(2),
                    });
                Ok(record)
            })
            .await
            .expect("rich record");
        registry.reconstruct(child.addr()).await.expect("rich child")
    }

    #[tokio::test]
    async fn relaunch_and_insert_preserve_complete_child_record() {
        let dir = tempfile::tempdir().expect("session database");
        let registry =
            ChildSessionRegistry::new(Arc::new(crate::memory::SqliteMemory::new(dir.path()).expect("sqlite")));
        let stale = rich_child(&registry).await;
        let before = registry.load_record(stale.addr()).await.expect("before");
        let run = registry.relaunch(stale.addr()).await.expect("relaunch").run;
        // A history snapshot predating relaunch must not overwrite its new run.
        registry.insert(&stale).await.expect("insert old snapshot");
        let after = registry.load_record(stale.addr()).await.expect("after");
        assert_eq!(
            after
                .child_session
                .as_ref()
                .and_then(|metadata| metadata.recipe.as_ref())
                .and_then(|recipe| recipe.last_run.clone()),
            run
        );
        let mut before = serde_json::to_value(before).expect("before JSON");
        let mut after = serde_json::to_value(after).expect("after JSON");
        for record in [&mut before, &mut after] {
            record.as_object_mut().expect("record").remove("updated_at");
            let last_run = record
                .get_mut("child_session")
                .and_then(|metadata| metadata.get_mut("recipe"))
                .and_then(|recipe| recipe.get_mut("last_run"))
                .expect("stored recipe last_run field");
            *last_run = serde_json::Value::Null;
        }
        assert_eq!(
            after, before,
            "title, creation, usage, summaries and full tool-call history must survive"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_history_insert_and_relaunch_keep_both_updates() {
        let dir = tempfile::tempdir().expect("session database");
        let first = ChildSessionRegistry::new(Arc::new(
            crate::memory::SqliteMemory::new(dir.path()).expect("first pool"),
        ));
        let second = ChildSessionRegistry::new(Arc::new(
            crate::memory::SqliteMemory::new(dir.path()).expect("independent pool"),
        ));
        let mut child = rich_child(&first).await;
        child.history.push(HistoryEntry {
            role: "user".into(),
            content: "prx appended turn".into(),
            timestamp: chrono::Utc::now(),
        });
        let address = child.addr.clone();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let insert_barrier = Arc::clone(&barrier);
        let insert = tokio::spawn(async move {
            insert_barrier.wait().await;
            first.insert(&child).await
        });
        barrier.wait().await;
        let run = second.relaunch(&address).await.expect("concurrent relaunch").run;
        insert.await.expect("insert task").expect("concurrent insert");
        let record = second.load_record(&address).await.expect("merged record");
        assert_eq!(record.turns.len(), 2);
        assert_eq!(record.turns.first().expect("original turn").tool_calls.len(), 1);
        assert_eq!(record.token_usage_records.len(), 1);
        assert_eq!(record.background_sessions.len(), 1);
        assert_eq!(
            record
                .child_session
                .and_then(|metadata| metadata.recipe)
                .and_then(|recipe| recipe.last_run),
            run
        );
    }
}
