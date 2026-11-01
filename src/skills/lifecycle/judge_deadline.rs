//! Deadline for one judge call.
//!
//! Production calls use `tokio::time`. A test can install a logical clock on
//! the store that owns the call: the deadline then elapses only when that
//! test advances it. The policy's `judge_timeout_ms` is unchanged.

use std::future::Future;
use std::time::Duration;

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::LazyLock;

#[cfg(test)]
use tokio::sync::watch;

/// How long `consult_judge` waits for the judge.
pub(crate) struct Limit {
    millis: u64,
    #[cfg(test)]
    clock: Option<Arc<JudgeClock>>,
}

impl Limit {
    pub(crate) const fn from_millis(millis: u64) -> Self {
        Self {
            millis,
            #[cfg(test)]
            clock: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_clock(mut self, clock: Option<Arc<JudgeClock>>) -> Self {
        self.clock = clock;
        self
    }

    /// Polls `call` first. The error means the deadline elapsed.
    pub(crate) async fn apply<T>(self, call: impl Future<Output = T>) -> Result<T, ()> {
        #[cfg(test)]
        if let Some(clock) = self.clock {
            return clock.wait(self.millis, call).await;
        }
        tokio::time::timeout(Duration::from_millis(self.millis), call)
            .await
            .map_err(|_| ())
    }
}

/// Logical milliseconds a test advances. Wall time does not move it.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct JudgeClock {
    elapsed_ms: watch::Sender<u64>,
}

#[cfg(test)]
impl JudgeClock {
    pub(crate) fn new() -> Arc<Self> {
        let (elapsed_ms, _) = watch::channel(0);
        Arc::new(Self { elapsed_ms })
    }

    pub(crate) fn advance(&self, by: Duration) {
        let add = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.elapsed_ms.send_modify(|slot| {
            *slot = slot.saturating_add(add);
        });
    }

    async fn wait<T>(&self, limit_ms: u64, call: impl Future<Output = T>) -> Result<T, ()> {
        tokio::pin!(call);
        let mut seen = self.elapsed_ms.subscribe();
        loop {
            if *seen.borrow() >= limit_ms {
                return Err(());
            }
            tokio::select! {
                biased;
                output = &mut call => return Ok(output),
                changed = seen.changed() => {
                    if changed.is_err() {
                        return Ok(call.await);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
static JUDGE_CLOCKS: LazyLock<parking_lot::Mutex<HashMap<usize, Arc<JudgeClock>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

#[cfg(test)]
fn store_key(store: &super::store::SkillLifecycleStore) -> usize {
    std::ptr::from_ref(store) as usize
}

/// Remembers `clock` for later `consult_judge` calls on this store, including
/// calls that run on a spawned stage worker. The worker and the test share
/// the service's `Arc` store, so the address stays stable.
#[cfg(test)]
pub(crate) fn install(store: &super::store::SkillLifecycleStore, clock: Arc<JudgeClock>) {
    JUDGE_CLOCKS.lock().insert(store_key(store), clock);
}

#[cfg(test)]
pub(crate) fn installed(store: &super::store::SkillLifecycleStore) -> Option<Arc<JudgeClock>> {
    JUDGE_CLOCKS.lock().get(&store_key(store)).cloned()
}

#[cfg(test)]
pub(crate) fn clear(store: &super::store::SkillLifecycleStore) {
    JUDGE_CLOCKS.lock().remove(&store_key(store));
}
