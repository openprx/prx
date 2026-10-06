#![cfg(test)]

//! Observe only jobs submitted while an owned fixture explicitly scopes its caller.
//! Receivers watch monitor completion; actual cleanup must also be drained independently.
use super::JobPhase;
use std::sync::Arc;

tokio::task_local! {
    static OBSERVED_JOBS: Arc<SubmittedJobs>;
}

#[derive(Default)]
pub(in crate::gateway) struct SubmittedJobs {
    phases: parking_lot::Mutex<Vec<tokio::sync::watch::Receiver<JobPhase>>>,
}

impl SubmittedJobs {
    pub(in crate::gateway) async fn scope<F: std::future::Future>(self: &Arc<Self>, work: F) -> F::Output {
        OBSERVED_JOBS.scope(Arc::clone(self), work).await
    }

    /// Call only after all submitting caller tasks were joined or aborted and joined.
    /// This observer never consumes a JobOutcome or changes a production phase.
    pub(in crate::gateway) async fn wait_terminal(
        &self,
        expected: usize,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<()> {
        let phases = std::mem::take(&mut *self.phases.lock());
        anyhow::ensure!(phases.len() == expected, "unexpected owned fixture submission count");
        for mut phase in phases {
            tokio::time::timeout_at(deadline, phase.wait_for(|value| value.is_terminal()))
                .await
                .map_err(|_| anyhow::anyhow!("owned fixture job terminal deadline exceeded"))?
                .map_err(|_| anyhow::anyhow!("owned fixture job terminal watch closed without confirmation"))?;
        }
        Ok(())
    }
}

pub(super) fn observe_submission(phase: &tokio::sync::watch::Receiver<JobPhase>) {
    let _ = OBSERVED_JOBS.try_with(|owner| owner.phases.lock().push(phase.clone()));
}
