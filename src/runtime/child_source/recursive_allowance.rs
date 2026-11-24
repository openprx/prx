//! A recursive publication allocates money from its run, never duplicates it.
use crate::runtime::turn_source::TurnSourceProof;
use std::collections::BTreeMap;
use std::sync::{
    Arc, OnceLock, Weak,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

type Registry = parking_lot::Mutex<BTreeMap<[u8; 32], Arc<RecursiveAllowance>>>;

// A config reload can publish a new service while a run is live. The run's
// allowance therefore belongs to the process, not a service.
static RUN_ALLOWANCES: OnceLock<Arc<Registry>> = OnceLock::new();

#[derive(Default)]
pub(in crate::runtime) struct RunAllowances(OnceLock<Arc<Registry>>);

pub(crate) struct RecursiveAllowance {
    money: Arc<AtomicU64>,
    retention: Option<Weak<Registry>>,
    contexts: AtomicUsize,
}

impl RecursiveAllowance {
    pub(crate) fn new(micro_usd: u64) -> Self {
        Self {
            money: Arc::new(AtomicU64::new(micro_usd)),
            retention: None,
            contexts: AtomicUsize::new(0),
        }
    }

    pub(crate) fn money(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.money)
    }

    /// An ordinary launch shares this run's counter: no child receives a
    /// private slice, so siblings never see 1/2, 1/4, 1/8 of the balance. The
    /// returned cents are the child's visible ceiling (the whole remaining
    /// balance, capped by `ceiling_cents`). The total stays enforced at spend
    /// time by the one atomic counter and the workspace cost journal.
    pub(crate) fn share_with_child(&self, ceiling_cents: u32) -> anyhow::Result<(u32, Arc<Self>)> {
        let available = self.money.load(Ordering::Acquire);
        let cents = u32::try_from(available.min(u64::from(ceiling_cents) * 10_000) / 10_000)?;
        // A zero-price child can still run with a zero money ceiling.
        // Paid requests remain denied by its accounting owner. Never give
        // it the parent's sub-cent remainder or manufacture a fresh budget.
        if cents == 0 {
            return Ok((0, Arc::new(Self::new(0))));
        }
        Ok((
            cents,
            Arc::new(Self {
                money: Arc::clone(&self.money),
                retention: None,
                contexts: AtomicUsize::new(0),
            }),
        ))
    }

    pub(in crate::runtime) fn retire(self: &Arc<Self>) {
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
    pub(in crate::runtime) fn existing(&self) -> Option<&Arc<Registry>> {
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

    pub(in crate::runtime) fn retain(&self, proof: &dyn TurnSourceProof) -> OnceLock<ContextAllowance> {
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

    pub(in crate::runtime) fn lookup(&self, proof: &dyn TurnSourceProof) -> Option<Arc<RecursiveAllowance>> {
        self.existing()?.lock().get(&Self::key(proof)).cloned()
    }

    pub(in crate::runtime) fn issue(&self, proof: &dyn TurnSourceProof, money: Arc<AtomicU64>) -> ContextAllowance {
        let registry = self
            .0
            .get_or_init(|| Arc::clone(RUN_ALLOWANCES.get_or_init(|| Arc::new(Registry::default()))));
        let key = Self::key(proof);
        let mut entries = registry.lock();
        ContextAllowance::new(Arc::clone(entries.entry(key).or_insert_with(|| {
            Arc::new(RecursiveAllowance {
                money,
                retention: Some(Arc::downgrade(registry)),
                contexts: AtomicUsize::new(0),
            })
        })))
    }
}

/// A registry entry is active only while a context owns it. Dropping admission on any error releases its lease.
pub(in crate::runtime) struct ContextAllowance(Arc<RecursiveAllowance>);
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
            // taking it so a concurrent lease cannot lose its registry entry.
            self.0.retire_if(|| self.0.contexts.load(Ordering::Acquire) == 0);
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod ordinary_concurrency_tests {
    use super::*;

    #[test]
    fn concurrent_ordinary_launches_share_one_counter() {
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
                    let (cents, child) = parent.share_with_child(1000).expect("test: share");
                    assert!(Arc::ptr_eq(&child.money(), &parent.money()));
                    cents
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), 1000);
        }
        assert_eq!(parent.money().load(Ordering::Acquire), initial);
    }
}
