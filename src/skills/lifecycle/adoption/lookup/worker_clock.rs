// SPDX-License-Identifier: MIT OR Apache-2.0
#![cfg(test)]

//! Independent, task-scoped regression hooks survive a missing clock re-entry.
use crate::causal_tree::logical_clock::ManualClock;
use std::future::Future;
use std::sync::Arc;

tokio::task_local! {
    static HOOK: (Arc<ManualClock>, &'static str);
}

pub(crate) fn scope<'a, F: Future + Send + 'a>(
    clock: Option<Arc<ManualClock>>,
    name: &'static str,
    work: F,
) -> std::pin::Pin<Box<dyn Future<Output = F::Output> + Send + 'a>> {
    // This box belongs to the test hook, keeping its task-local wrapper out
    // of the already large stage-worker future's concrete outer type.
    Box::pin(async move {
        match clock {
            Some(clock) => HOOK.scope((clock, name), work).await,
            None => work.await,
        }
    })
}

pub(crate) fn sync_scope<R>(clock: Option<Arc<ManualClock>>, name: &'static str, work: impl FnOnce() -> R) -> R {
    match clock {
        Some(clock) => HOOK.sync_scope((clock, name), work),
        None => work(),
    }
}

pub(super) fn apply() {
    let _ = HOOK.try_with(|(clock, name)| clock.visit_site(name));
}

#[tokio::test]
async fn worker_hooks_preserve_logical_expiry_and_do_not_install_or_leak_a_clock() {
    use super::{LessonLimits, LookupBudget};
    use crate::causal_tree::logical_clock;
    use std::time::Duration;
    for name in ["experiment.stage-lookup", "experiment.preparation-lookup"] {
        let clock = ManualClock::new();
        clock.arm_site(name, Duration::ZERO, Duration::from_millis(501));
        let result = logical_clock::scope(
            Arc::clone(&clock),
            scope(Some(Arc::clone(&clock)), name, async {
                LookupBudget::new(LessonLimits::default()).unwrap().check()
            }),
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("lesson lookup deadline exhausted; no partial snapshot is usable")
        );
        assert_eq!(clock.site_hits(name), 1);
        assert!(!logical_clock::installed());
        // A different task has no inherited hook and cannot spend this clock.
        tokio::spawn(async {
            LookupBudget::new(LessonLimits::default()).unwrap().check().unwrap();
        })
        .await
        .unwrap();
        assert_eq!(clock.site_hits(name), 1);
    }
    scope(None, "default-worker", async {
        assert!(!logical_clock::installed());
        let mut budget = LookupBudget::new(LessonLimits::default()).unwrap();
        std::thread::sleep(Duration::from_millis(550));
        assert!(
            budget
                .check()
                .unwrap_err()
                .to_string()
                .contains("lesson lookup deadline exhausted")
        );
    })
    .await;
}
