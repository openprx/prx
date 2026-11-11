//! Receiver-owned live execution/lease state, shared by every memory clone.
use crate::messaging::SessionAddr;
use crate::messaging::mailbox::{MailboxAuthority, MailboxError, MailboxResult, MailboxScope, natural_key};
use parking_lot::{Condvar, Mutex};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};
use uuid::Uuid;

/// Independently opened pools in the receiving process must share the same
/// lease registry; otherwise an erase through a second handle could miss IPC.
pub(in crate::memory::sqlite) fn for_database(path: &std::path::Path) -> anyhow::Result<Arc<Runtime>> {
    type Registry = Mutex<BTreeMap<std::path::PathBuf, std::sync::Weak<Runtime>>>;
    static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    let identity = std::fs::canonicalize(path)?;
    let mut entries = REGISTRY.get_or_init(Mutex::default).lock();
    entries.retain(|_, runtime| runtime.strong_count() > 0);
    if let Some(runtime) = entries.get(&identity).and_then(std::sync::Weak::upgrade) {
        return Ok(runtime);
    }
    let runtime = Arc::new(Runtime::default());
    entries.insert(identity, Arc::downgrade(&runtime));
    Ok(runtime)
}

#[derive(Default)]
pub(in crate::memory::sqlite) struct Runtime {
    state: Mutex<State>,
    changed: Condvar,
}
#[derive(Default)]
struct State {
    leases: BTreeMap<Uuid, (usize, usize)>,
    executions: HashMap<SessionAddr, (MailboxScope, String)>,
    rate: HashMap<Vec<u8>, (i64, u8)>,
    swept_at: Option<i64>,
}

pub(in crate::memory::sqlite) struct Lease {
    runtime: Arc<Runtime>,
    scope: Uuid,
    cache: bool,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut state = self.runtime.state.lock();
        if let Some(count) = state.leases.get_mut(&self.scope) {
            if self.cache {
                count.1 = count.1.saturating_sub(1);
            } else {
                count.0 = count.0.saturating_sub(1);
            }
            if *count == (0, 0) {
                state.leases.remove(&self.scope);
            }
        }
        self.runtime.changed.notify_all();
    }
}
impl Runtime {
    pub fn lease(self: &Arc<Self>, scope: Uuid) -> Lease {
        let mut state = self.state.lock();
        state.leases.entry(scope).or_default().0 += 1;
        Lease {
            runtime: Arc::clone(self),
            scope,
            cache: false,
        }
    }
    pub fn cache_lease(self: &Arc<Self>, scope: Uuid) -> Lease {
        self.state.lock().leases.entry(scope).or_default().1 += 1;
        Lease {
            runtime: Arc::clone(self),
            scope,
            cache: true,
        }
    }
    pub fn set_execution(&self, addr: SessionAddr, scope: MailboxScope, run: Option<String>) {
        let mut state = self.state.lock();
        if let Some(run) = run {
            state.executions.insert(addr, (scope, run));
        } else if state.executions.get(&addr).is_some_and(|(bound, _)| bound == &scope) {
            state.executions.remove(&addr);
        }
    }
    pub fn close_execution(&self, addr: &SessionAddr, scope: &MailboxScope, run: &str) {
        let mut state = self.state.lock();
        if state
            .executions
            .get(addr)
            .is_some_and(|(bound, current)| bound == scope && current == run)
        {
            state.executions.remove(addr);
        }
    }
    pub fn execution(&self, addr: &SessionAddr, scope: &MailboxScope) -> Option<String> {
        self.state
            .lock()
            .executions
            .get(addr)
            .filter(|(bound, _)| bound == scope)
            .map(|(_, run)| run.clone())
    }
    pub fn has_leases(&self, scope: Uuid) -> bool {
        self.state.lock().leases.get(&scope).is_some_and(|counts| counts.0 > 0)
    }
    pub fn has_scope_leases(&self, scope: Uuid) -> bool {
        self.state.lock().leases.contains_key(&scope)
    }
    pub fn drain_and_join(&self, scope: Uuid) {
        let mut state = self.state.lock();
        state.executions.retain(|_, (bound, _)| bound.scope_id != scope);
        while state.leases.contains_key(&scope) {
            self.changed.wait(&mut state);
        }
    }
    pub fn check_rate(&self, authority: &MailboxAuthority, now: i64) -> MailboxResult<()> {
        let mut state = self.state.lock();
        if state.swept_at.is_none_or(|last| now > last) {
            state.rate.retain(|_, (second, _)| *second >= now);
            state.swept_at = Some(now);
        }
        for key in rate_keys(authority)? {
            if state
                .rate
                .get(&key)
                .is_some_and(|(second, count)| *second >= now && *count >= 10)
            {
                return Err(MailboxError::RateLimited);
            }
        }
        Ok(())
    }
    /// Called after COMMIT while still holding the shared writer guard.
    pub fn record_rate(&self, authority: &MailboxAuthority, now: i64) -> MailboxResult<()> {
        let mut state = self.state.lock();
        for key in rate_keys(authority)? {
            let bucket = state.rate.entry(key).or_insert((now, 0));
            if now > bucket.0 {
                *bucket = (now, 0);
            }
            bucket.1 = bucket.1.saturating_add(1);
        }
        Ok(())
    }
}
fn rate_keys(authority: &MailboxAuthority) -> MailboxResult<[Vec<u8>; 2]> {
    let peer = authority.auth.as_ref().map_or(
        [authority.source.instance.as_str(), authority.source.owner.as_str()],
        |auth| [auth.issuer.as_str(), auth.subject.as_str()],
    );
    Ok([
        natural_key(b"prx-mailbox-rate-peer", &peer)?,
        natural_key(
            b"prx-mailbox-rate-session",
            &[
                &authority.workspace_id,
                &authority.destination.owner,
                &authority.destination.session,
                &authority.destination.agent,
            ],
        )?,
    ])
}
