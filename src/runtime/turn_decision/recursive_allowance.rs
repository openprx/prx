//! A recursive publication allocates money from its run, never duplicates it.
use super::{TurnDecisionContext, TurnDecisionService, TurnSourceProof};
use std::collections::BTreeMap;
use std::sync::{
    Arc, OnceLock, Weak,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

type Registry = parking_lot::Mutex<BTreeMap<[u8; 32], Arc<RecursiveAllowance>>>;

// A config reload can publish a new service while an approval is pending.
// The original run's allowance therefore belongs to the process, not a service.
static RUN_ALLOWANCES: OnceLock<Arc<Registry>> = OnceLock::new();

#[derive(Default)]
pub(super) struct RunAllowances(OnceLock<Arc<Registry>>);

pub(crate) struct RecursiveAllowance {
    money: Arc<AtomicU64>,
    claimed: AtomicBool,
    funded: bool,
    retention: Option<Weak<Registry>>,
    contexts: AtomicUsize,
    pending_until: parking_lot::Mutex<Option<std::time::Instant>>,
}

impl RecursiveAllowance {
    pub(crate) fn new(micro_usd: u64) -> Self {
        Self {
            money: Arc::new(AtomicU64::new(micro_usd)),
            claimed: AtomicBool::new(false),
            funded: true,
            retention: None,
            contexts: AtomicUsize::new(0),
            pending_until: parking_lot::Mutex::new(None),
        }
    }

    pub(crate) fn money(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.money)
    }

    pub(crate) fn funded_money(&self) -> Option<Arc<AtomicU64>> {
        self.funded.then(|| self.money())
    }

    pub(crate) fn ensure_available(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.funded, "original recursive allowance is unavailable after resume");
        anyhow::ensure!(
            !self.claimed.load(Ordering::Acquire),
            "recursive grant already allocated its one child allowance"
        );
        Ok(())
    }

    /// Claim once and subtract a whole-cent strict share before publication.
    /// A failed publication keeps its allocation consumed, never reissues it.
    pub(crate) fn carve_child(&self, ceiling_cents: u32) -> anyhow::Result<(u32, Arc<Self>)> {
        anyhow::ensure!(self.funded, "original recursive allowance is unavailable after resume");
        anyhow::ensure!(
            self.claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "recursive grant already allocated its one child allowance"
        );
        let allocation = self.carve_ordinary_child(ceiling_cents)?;
        anyhow::ensure!(
            allocation.0 > 0,
            "recursive parent allowance cannot fund a strict whole-cent child share"
        );
        Ok(allocation)
    }

    /// Each ordinary launch atomically reserves a strict share of the balance.
    /// Failed launches retain their reservation; siblings cannot reuse it.
    pub(crate) fn carve_ordinary_child(&self, ceiling_cents: u32) -> anyhow::Result<(u32, Arc<Self>)> {
        anyhow::ensure!(self.funded, "parent child allowance is unavailable");
        let mut available = self.money.load(Ordering::Acquire);
        loop {
            let cents = u32::try_from(available.min(u64::from(ceiling_cents) * 10_000) / 20_000)?;
            // A zero-price child can still run with a zero money ceiling.
            // Paid requests remain denied by its accounting owner. Never give
            // it the parent's sub-cent remainder or manufacture a fresh budget.
            if cents == 0 {
                return Ok((0, Arc::new(Self::new(0))));
            }
            let share = u64::from(cents) * 10_000;
            match self
                .money
                .compare_exchange_weak(available, available - share, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Ok((cents, Arc::new(Self::new(share)))),
                Err(current) => available = current,
            }
        }
    }

    fn retire(self: &Arc<Self>) {
        self.retire_if(|| true);
    }

    fn retire_if(self: &Arc<Self>, predicate: impl FnOnce() -> bool) {
        if let Some(registry) = &self.retention
            && let Some(registry) = registry.upgrade()
        {
            let mut registry = registry.lock();
            if predicate() {
                registry.retain(|_, entry| !Arc::ptr_eq(entry, self));
            }
        }
    }
}

impl RunAllowances {
    fn existing(&self) -> Option<&Arc<Registry>> {
        self.0.get().or_else(|| RUN_ALLOWANCES.get())
    }

    fn key(proof: &dyn TurnSourceProof) -> [u8; 32] {
        Self::key_for_run(proof, proof.run_id())
    }

    fn key_for_run(proof: &dyn TurnSourceProof, run: &str) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for part in [proof.workspace_id(), proof.owner_id(), proof.session_key(), run] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        hash.finalize().into()
    }

    pub(super) fn retain(&self, proof: &dyn TurnSourceProof) -> OnceLock<ContextAllowance> {
        let Some(registry) = self.existing() else {
            return OnceLock::new();
        };
        let entries = registry.lock();
        entries
            .get(&Self::key(proof))
            .cloned()
            .map(ContextAllowance::new)
            .map_or_else(OnceLock::new, OnceLock::from)
    }

    pub(super) fn inherit_resume(&self, proof: &dyn TurnSourceProof, original_run: &str) -> ContextAllowance {
        let registry = self
            .0
            .get_or_init(|| Arc::clone(RUN_ALLOWANCES.get_or_init(|| Arc::new(Registry::default()))));
        let mut entries = registry.lock();
        let original = entries
            .get(&Self::key_for_run(proof, original_run))
            .cloned()
            .unwrap_or_else(|| {
                // A process restart has lost the admitted allowance. Fail
                // closed rather than reconstructing an unused child slot.
                Arc::new(RecursiveAllowance {
                    money: Arc::new(AtomicU64::new(0)),
                    claimed: AtomicBool::new(true),
                    funded: false,
                    retention: Some(Arc::downgrade(registry)),
                    contexts: AtomicUsize::new(0),
                    pending_until: parking_lot::Mutex::new(None),
                })
            });
        *original.pending_until.lock() = None;
        entries.insert(Self::key(proof), Arc::clone(&original));
        ContextAllowance::new(original)
    }

    fn lookup(&self, proof: &dyn TurnSourceProof) -> Option<Arc<RecursiveAllowance>> {
        self.existing()?.lock().get(&Self::key(proof)).cloned()
    }

    fn issue(&self, proof: &dyn TurnSourceProof, money: Arc<AtomicU64>) -> ContextAllowance {
        let registry = self
            .0
            .get_or_init(|| Arc::clone(RUN_ALLOWANCES.get_or_init(|| Arc::new(Registry::default()))));
        let key = Self::key(proof);
        let mut entries = registry.lock();
        ContextAllowance::new(Arc::clone(entries.entry(key).or_insert_with(|| {
            Arc::new(RecursiveAllowance {
                money,
                claimed: AtomicBool::new(false),
                funded: true,
                retention: Some(Arc::downgrade(registry)),
                contexts: AtomicUsize::new(0),
                pending_until: parking_lot::Mutex::new(None),
            })
        })))
    }
}

impl TurnDecisionService {
    pub(super) fn recursive_money(&self, proof: &dyn TurnSourceProof) -> Option<Arc<AtomicU64>> {
        if let Some(incoming) = &self.parent_grant {
            return incoming
                .recursive_child_allowance()
                .and_then(|owner| owner.funded_money());
        }
        if let Some(money) = &self.ordinary_money {
            return Some(Arc::clone(money));
        }
        self.recursive_allowances
            .lookup(proof)
            .and_then(|owner| owner.funded_money())
    }

    pub(super) fn issue_recursive_allowance(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> Option<Arc<RecursiveAllowance>> {
        if let Some(incoming) = &self.parent_grant {
            return incoming.recursive_child_allowance();
        }
        Some(Arc::clone(context.recursive_allowance.get_or_init(|| {
            let money = context.accounting.as_ref().map_or_else(
                || {
                    self.recursive_money(proof).unwrap_or_else(|| {
                        Arc::new(AtomicU64::new(context.engine.state.trusted.parts().budgets.micro_usd))
                    })
                },
                |scope| scope.money_owner(),
            );
            self.recursive_allowances.issue(proof, money)
        })))
    }
}

impl TurnDecisionContext {
    pub(crate) fn retire_recursive_allowance(&self) {
        if let Some(owner) = self.recursive_allowance.get() {
            owner.retire();
        }
    }
}

/// A registry entry is active only while a context or a durable pending
/// approval owns it. Dropping admission on any error releases its lease.
pub(super) struct ContextAllowance(Arc<RecursiveAllowance>);
impl ContextAllowance {
    fn new(owner: Arc<RecursiveAllowance>) -> Self {
        owner.contexts.fetch_add(1, Ordering::AcqRel);
        Self(owner)
    }
}
impl std::ops::Deref for ContextAllowance {
    type Target = Arc<RecursiveAllowance>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Drop for ContextAllowance {
    fn drop(&mut self) {
        let last = self.0.contexts.fetch_sub(1, Ordering::AcqRel) == 1;
        if last {
            // Lease acquisition holds the same registry lock. Recheck after
            // taking it so a concurrent resume cannot lose its registry entry.
            self.0
                .retire_if(|| self.0.contexts.load(Ordering::Acquire) == 0 && self.0.pending_until.lock().is_none());
        }
    }
}

impl RecursiveAllowance {
    pub(super) fn retain_pending(self: &Arc<Self>, expires_at: chrono::DateTime<chrono::Utc>) {
        let remaining = (expires_at - chrono::Utc::now()).to_std().unwrap_or_default();
        let Some(deadline) = std::time::Instant::now().checked_add(remaining) else {
            self.retire();
            return;
        };
        *self.pending_until.lock() = Some(deadline);
        let owner = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline.into()).await;
            if let Some(owner) = owner.upgrade() {
                // Serialize expiry with resume's alias/lease publication.
                owner.retire_if(|| {
                    let mut pending = owner.pending_until.lock();
                    if *pending == Some(deadline) {
                        *pending = None;
                        true
                    } else {
                        false
                    }
                });
            }
        });
    }
}

impl RunAllowances {
    pub(super) fn retire_pending(&self, workspace: &str, owner: &str, session: &str, run: &str) {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for part in [workspace, owner, session, run] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        let key: [u8; 32] = hash.finalize().into();
        let entry = self.existing().and_then(|registry| registry.lock().get(&key).cloned());
        if let Some(entry) = entry {
            *entry.pending_until.lock() = None;
            entry.retire();
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ordinary_concurrency_tests {
    use super::*;

    #[test]
    fn concurrent_ordinary_reservations_conserve_the_original_envelope() {
        let initial = 10_000_000;
        let parent = Arc::new(RecursiveAllowance::new(initial));
        let barrier = Arc::new(std::sync::Barrier::new(16));
        // Every barrier participant must start before any thread is joined.
        #[allow(clippy::needless_collect)]
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let parent = Arc::clone(&parent);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    parent.carve_ordinary_child(1000).map_or(0, |(cents, child)| {
                        let share = u64::from(cents) * 10_000;
                        assert_eq!(child.money().load(Ordering::Acquire), share);
                        share
                    })
                })
            })
            .collect();
        let reserved: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert!(reserved > 0);
        assert_eq!(reserved + parent.money().load(Ordering::Acquire), initial);
    }
}
