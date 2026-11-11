//! ChildSession ownership object (TUI-1a). Not wired to keys. Mailbox-cursor
//! reconstruction is MSG-CORE.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use parking_lot::Mutex;
use uuid::Uuid;

use super::approval::PendingApprovals;
use crate::messaging::{ExecRef, InheritedProvenance, SessionAddr};
use crate::tools::sessions_spawn::HistoryEntry;

/// Inbound mailbox consume cursor. MSG-CORE fills reconstruction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailboxCursor {}

/// Provider binding without credentials (no key wiring).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderBinding {
    /// Provider name.
    pub provider: String,
    /// Model id.
    pub model: String,
}

/// Process-mode relaunch recipe: same [`SessionAddr`], new run.
#[derive(Debug, Clone)]
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

    fn snapshot(&self) -> ChildSessionRecord {
        ChildSessionRecord {
            addr: self.addr.clone(),
            history: self.history.clone(),
            provider: self.provider.clone(),
            recipe: self.recipe.clone(),
        }
    }
}

#[derive(Clone)]
struct ChildSessionRecord {
    addr: SessionAddr,
    history: Vec<HistoryEntry>,
    provider: Option<ProviderBinding>,
    recipe: Option<RelaunchRecipe>,
}

/// Durable-enough registry used to reconstruct ChildSession objects.
#[derive(Clone, Default)]
pub struct ChildSessionRegistry {
    inner: Arc<Mutex<HashMap<SessionAddr, ChildSessionRecord>>>,
}

impl ChildSessionRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a child session record.
    pub fn insert(&self, session: &ChildSession) {
        self.inner.lock().insert(session.addr.clone(), session.snapshot());
    }

    /// Reconstruct from the registry only (no mailbox cursor, no live worker).
    /// In-memory Ask does not survive.
    pub fn reconstruct(&self, addr: &SessionAddr) -> Result<ChildSession> {
        let record = self
            .inner
            .lock()
            .get(addr)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("child session not in registry"))?;
        Ok(ChildSession {
            addr: record.addr,
            history: record.history,
            provider: record.provider,
            mailbox_cursor: MailboxCursor::default(),
            approvals: PendingApprovals::new(),
            turn: Mutex::new(()),
            recipe: record.recipe,
        })
    }

    /// Relaunch a process child: same [`SessionAddr`], new [`ExecRef::run`].
    pub fn relaunch(&self, addr: &SessionAddr) -> Result<ExecRef> {
        let mut guard = self.inner.lock();
        let record = guard
            .get_mut(addr)
            .ok_or_else(|| anyhow::anyhow!("child session not in registry"))?;
        let recipe = record
            .recipe
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("child session has no relaunch recipe"))?;
        if recipe.addr != *addr {
            bail!("relaunch recipe address mismatch");
        }
        let run = Uuid::now_v7().to_string();
        recipe.last_run = Some(run.clone());
        Ok(ExecRef { run: Some(run) })
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

    #[test]
    fn child_session_reconstructs_from_registry() {
        let registry = ChildSessionRegistry::new();
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
        registry.insert(&live);
        drop(live);
        let restored = registry.reconstruct(&addr("child-a")).expect("test: reconstruct");
        assert_eq!(restored.addr(), &addr("child-a"));
        assert_eq!(restored.history().len(), 1);
        assert_eq!(restored.provider().map(|p| p.provider.as_str()), Some("openrouter"));
        assert!(!restored.approvals().is_pending("ask-1"), "in-memory Ask dies");
        assert_eq!(restored.mailbox_cursor(), &MailboxCursor::default());
    }

    #[test]
    fn relaunch_keeps_session_addr_and_mints_new_run() {
        let registry = ChildSessionRegistry::new();
        let session = ChildSession::new(addr("child-a"), None, None);
        registry.insert(&session);
        let first = registry.relaunch(&addr("child-a")).expect("test: first");
        let second = registry.relaunch(&addr("child-a")).expect("test: second");
        assert_ne!(first.run, second.run);
        assert!(first.run.is_some());
        let restored = registry.reconstruct(&addr("child-a")).expect("test: reconstruct");
        assert_eq!(restored.addr(), &addr("child-a"));
        assert_eq!(restored.recipe().and_then(|r| r.last_run.clone()), second.run);
    }
}
