//! ChildSession ownership object (TUI-1a). Not wired to keys. Mailbox-cursor
//! reconstruction is MSG-CORE.

use std::sync::Arc;

use anyhow::{Result, bail};
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

/// Child identity and relaunch metadata stored in the existing chat-session record.
/// History uses the record's normal turns; approvals and live handles are never saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChildSessionMetadata {
    pub addr: SessionAddr,
    pub provider: Option<ProviderBinding>,
    pub recipe: Option<RelaunchRecipe>,
}

/// Child view over the existing durable chat-session registry, without a second map.
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

    /// Persist a child's metadata and history through the existing session store.
    pub async fn insert(&self, child: &ChildSession) -> Result<()> {
        let mut record = crate::chat::session::ChatSession::new(
            child.provider.as_ref().map_or("", |p| p.provider.as_str()),
            child.provider.as_ref().map_or("", |p| p.model.as_str()),
        );
        record.id = Self::record_id(child.addr())?;
        record.child_session = Some(child.metadata());
        record.turns = child
            .history
            .iter()
            .map(|entry| crate::chat::session::ChatTurn {
                role: entry.role.clone(),
                content: entry.content.clone(),
                timestamp: entry.timestamp,
                tool_calls: Vec::new(),
            })
            .collect();
        crate::chat::session_store::save_session(self.memory.as_ref(), &record).await
    }

    /// Reopen the durable registry record only. Ask dies; no worker is revived.
    pub async fn reconstruct(&self, addr: &SessionAddr) -> Result<ChildSession> {
        let record = crate::chat::session_store::load_session_by_id(self.memory.as_ref(), &Self::record_id(addr)?)
            .await?
            .ok_or_else(|| anyhow::anyhow!("child session not in registry"))?;
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

    /// Persist a fresh execution reference for the same logical child address.
    pub async fn relaunch(&self, addr: &SessionAddr) -> Result<ExecRef> {
        let mut child = self.reconstruct(addr).await?;
        let recipe = child
            .recipe
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("child session has no relaunch recipe"))?;
        if recipe.addr != *addr {
            bail!("relaunch recipe address mismatch");
        }
        let run = Uuid::now_v7().to_string();
        recipe.last_run = Some(run.clone());
        self.insert(&child).await?;
        Ok(ExecRef { run: Some(run) })
    }

    /// Build the relaunch manifest from registry provenance. The caller seals
    /// the complete new manifest HMAC after supplying fresh worker authority.
    pub async fn relaunch_worker(
        &self,
        addr: &SessionAddr,
        mut manifest: crate::session_worker::protocol::WorkerManifest,
    ) -> Result<crate::session_worker::protocol::WorkerManifest> {
        let exec = self.relaunch(addr).await?;
        let child = self.reconstruct(addr).await?;
        let recipe = child.recipe.ok_or_else(|| anyhow::anyhow!("relaunch recipe missing"))?;
        let run = exec.run.ok_or_else(|| anyhow::anyhow!("relaunch returned no run"))?;
        anyhow::ensure!(
            recipe.last_run.as_deref() == Some(run.as_str()),
            "relaunch registry run mismatch"
        );
        manifest.run_id = run;
        manifest.session_scope_key = addr.session.clone();
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
}
