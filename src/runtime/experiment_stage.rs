//! An admitted lifecycle stage borrows the existing physical accounting owner.
//! Source admission and every ordinary tool/policy check remain mandatory.
use super::{decision_accounting::AccountingScope, turn_cleanup::current_tracker};
use crate::{
    config::Config,
    decision::DecisionKey,
    skills::lifecycle::experiment::{ExperimentResources, FrozenExperiment},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio_util::sync::CancellationToken;

tokio::task_local! { static CURRENT: Arc<ExperimentStageRuntime>; }
pub(crate) struct ExperimentStageRuntime {
    pub(super) accounting: Arc<AccountingScope>,
    key: Arc<DecisionKey>,
    cancel: CancellationToken,
    tools: AtomicU64,
    resources: ExperimentResources,
    output: AtomicU64,
    scratch: AtomicU64,
    files: files::ScratchWrites,
    binding: crate::skills::lifecycle::experiment::ExperimentRunBinding,
    store: Arc<crate::skills::lifecycle::SkillLifecycleStore>,
    id: crate::skills::lifecycle::experiment::ExperimentId,
}
impl ExperimentStageRuntime {
    pub(crate) fn physical_identity(key: &DecisionKey, digest: &str, stage: &str) -> [u8; 32] {
        key.hmac(
            crate::decision::KeyDomain::TerminalClaim,
            &[b"instruction-experiment-stage-v1", digest.as_bytes(), stage.as_bytes()],
        )
    }
    pub(crate) fn new(
        config: &Config,
        plan: &FrozenExperiment,
        stage: &str,
        cancel: CancellationToken,
        store: Arc<crate::skills::lifecycle::SkillLifecycleStore>,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(current().is_none(), "nested experiment stage admission is forbidden");
        plan.ensure_live(chrono::Utc::now())?;
        let admitted = plan
            .spec()
            .stages
            .iter()
            .find(|value| value.id == stage)
            .ok_or_else(|| anyhow::anyhow!("unknown experiment stage"))?;
        let remaining_ms = u64::try_from(
            plan.spec()
                .expires_at
                .signed_duration_since(chrono::Utc::now())
                .num_milliseconds(),
        )?;
        let key = DecisionKey::load_or_create(&config.workspace_dir)?;
        let identity = Self::physical_identity(&key, plan.digest(), stage);
        let resources = admitted.allocation;
        let accounting = AccountingScope::new(
            Arc::new(config.clone()),
            Arc::clone(&key),
            current_tracker().ok_or_else(|| anyhow::anyhow!("experiment stage has no cleanup owner"))?,
            identity,
            crate::causal_tree::state::TurnBudgets {
                tokens: resources.tokens,
                micro_usd: resources.micro_usd,
                latency_ms: remaining_ms,
            },
            Instant::now()
                .checked_add(std::time::Duration::from_millis(remaining_ms))
                .ok_or_else(|| anyhow::anyhow!("experiment deadline exceeds monotonic clock range"))?,
        )
        .with_call_limit(resources.calls)
        .with_group(crate::cost::reservation::ReservationGroup {
            id: plan.digest().into(),
            calls: plan.spec().resources.calls,
            tokens: plan.spec().resources.tokens,
            micro_usd: plan.spec().resources.micro_usd,
        });
        let runtime = Arc::new(Self {
            accounting: Arc::new(accounting),
            key,
            store,
            id: plan.spec().id.clone(),
            cancel,
            tools: AtomicU64::new(0),
            resources,
            output: AtomicU64::new(0),
            scratch: AtomicU64::new(0),
            files: files::ScratchWrites::default(),
            binding: crate::skills::lifecycle::experiment::ExperimentRunBinding {
                experiment_sha256: plan.digest().into(),
                stage: stage.into(),
                input_sha256: admitted.input_sha256.clone(),
            },
        });
        runtime.accounting.bind_stage(&runtime)?;
        Ok(runtime)
    }
    /// The durable cancel command reaches an in-flight stage in another
    /// process. This watcher is owned by the same cleanup tracker as its work.
    pub(crate) fn watch(self: &Arc<Self>) -> anyhow::Result<()> {
        let stop = CancellationToken::new();
        let ticket = current_tracker()
            .ok_or_else(|| anyhow::anyhow!("stage watcher has no cleanup owner"))?
            .register(stop.clone())?;
        let stage = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(100));
            let mut confirmed = true;
            loop {
                tokio::select! {
                    biased;
                    () = stop.cancelled() => break,
                    _ = interval.tick() => {
                        let read = Arc::clone(&stage);
                        let status = tokio::task::spawn_blocking(move || read.ensure_active()).await;
                        if !matches!(status, Ok(Ok(()))) {
                            // Cancellation/expiry is normal; unreadable authority
                            // is conservative too and remains in the journal.
                            if status.is_err() { confirmed = false; }
                            stage.cancel.cancel(); break;
                        }
                    }
                }
            }
            ticket.finish(confirmed);
        });
        Ok(())
    }
    pub(crate) async fn flush(&self) -> anyhow::Result<()> {
        self.accounting.flush().await
    }
    /// Own only a heap pointer in the task-local wrapper. Carrying the entire
    /// agent future in each async wrapper duplicates large poll stack frames.
    pub(crate) fn scope<F: std::future::Future>(
        self: &Arc<Self>,
        future: F,
    ) -> impl std::future::Future<Output = F::Output> + use<F> {
        CURRENT.scope(Arc::clone(self), Box::pin(future))
    }
    pub(crate) fn claim_tool(&self, name: &str) -> anyhow::Result<()> {
        self.ensure_active()?;
        // Process and recursive adapters need explicit containment/allowance
        // bridges. They cannot accidentally inherit a task-local allowance.
        anyhow::ensure!(
            !matches!(name, "delegate" | "sessions_spawn" | "shell"),
            "experiment stage adapter is not admitted for {name}"
        );
        self.tools
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1).filter(|next| *next <= self.resources.tool_calls)
            })
            .map_err(|_| anyhow::anyhow!("experiment tool-call allowance exhausted"))?;
        Ok(())
    }
    pub(crate) fn account_output(&self, bytes: usize) -> anyhow::Result<()> {
        let bytes = u64::try_from(bytes)?;
        let previous = self
            .output
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| value.checked_add(bytes))
            .map_err(|_| anyhow::anyhow!("experiment output counter overflow"))?;
        anyhow::ensure!(
            previous + bytes <= self.resources.output_bytes,
            "experiment output allowance exhausted"
        );
        Ok(())
    }
    pub(crate) fn account_scratch(&self, bytes: u64) -> anyhow::Result<()> {
        self.scratch.fetch_max(bytes, Ordering::AcqRel);
        anyhow::ensure!(
            bytes <= self.resources.scratch_bytes,
            "experiment scratch allowance exhausted"
        );
        Ok(())
    }
    pub(crate) fn scratch_bytes(&self) -> u64 {
        self.scratch.load(Ordering::Acquire).max(self.written_scratch_bytes())
    }
    pub(crate) fn binding(&self) -> crate::skills::lifecycle::experiment::ExperimentRunBinding {
        self.binding.clone()
    }
    pub(crate) fn scope_id(&self) -> String {
        self.accounting.scope_id()
    }
    pub(crate) fn tool_count(&self) -> u64 {
        self.tools.load(Ordering::Acquire)
    }
    pub(crate) fn output_bytes(&self) -> u64 {
        self.output.load(Ordering::Acquire)
    }
    pub(crate) fn ensure_active(&self) -> anyhow::Result<()> {
        let journal = self.store.read_experiment(&self.id, &self.binding.experiment_sha256)?;
        journal.plan.ensure_live(chrono::Utc::now())?;
        if journal.state.cancelled {
            self.cancel.cancel();
        }
        anyhow::ensure!(
            matches!(
                journal.state.stages.get(&self.binding.stage),
                Some(crate::skills::lifecycle::experiment::StageState::Dispatched)
            ),
            "experiment stage is not durably dispatched"
        );
        anyhow::ensure!(
            self.scratch.load(Ordering::Acquire) <= self.resources.scratch_bytes,
            "experiment scratch allowance exhausted"
        );
        anyhow::ensure!(
            self.output.load(Ordering::Acquire) <= self.resources.output_bytes,
            "experiment output allowance exhausted"
        );
        anyhow::ensure!(
            !self.cancel.is_cancelled() && Instant::now() < self.accounting.deadline(),
            "experiment stage stopped"
        );
        Ok(())
    }
}
pub(crate) fn current() -> Option<Arc<ExperimentStageRuntime>> {
    CURRENT.try_with(Arc::clone).ok()
}
pub(crate) fn retain<F: std::future::Future>(
    stage: Option<Arc<ExperimentStageRuntime>>,
    future: F,
) -> impl std::future::Future<Output = F::Output> + use<F> {
    let future = Box::pin(future);
    async move {
        match stage {
            Some(stage) => CURRENT.scope(stage, future).await,
            None => future.await,
        }
    }
}

#[cfg(test)]
mod tests;

mod chat;
mod files;
pub(crate) use chat::chat_traced;
