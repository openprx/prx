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
    continuation: Option<super::h2_entry::RunContinuation>,
    workspace: std::path::PathBuf,
    case_template_sha256: String,
    configuration: Arc<Config>,
    pub(super) accounting: Arc<AccountingScope>,
    key: Arc<DecisionKey>,
    cancel: CancellationToken,
    tools: AtomicU64,
    resources: ExperimentResources,
    kind: crate::skills::lifecycle::experiment::ExperimentStageKind,
    gate_subject: std::sync::OnceLock<crate::skills::lifecycle::GateReviewSubject>,
    output: AtomicU64,
    scratch: AtomicU64,
    files: files::ScratchWrites,
    binding: crate::skills::lifecycle::experiment::ExperimentRunBinding,
    store: Arc<crate::skills::lifecycle::SkillLifecycleStore>,
    id: crate::skills::lifecycle::experiment::ExperimentId,
}
impl ExperimentStageRuntime {
    fn case_template_digest(config: &Config) -> anyhow::Result<String> {
        use sha2::{Digest, Sha256};
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&serde_json::json!({
            "domain":"prx-admitted-case-template-v1","configuration":config,
            "workspace":config.workspace_dir,"config_path":config.config_path,
        }))?)))
    }
    pub(crate) fn verify_case_template(&self, config: &Config) -> anyhow::Result<()> {
        self.ensure_active()?;
        anyhow::ensure!(
            Self::case_template_digest(config)? == self.case_template_sha256,
            "controlled case template differs from the actual admitted stage configuration"
        );
        Ok(())
    }
    pub(crate) fn verified_experiment_origin(
        &self,
        owner: &crate::skills::lifecycle::OwnerScope,
    ) -> anyhow::Result<(crate::skills::lifecycle::experiment::ExperimentId, String)> {
        self.ensure_active()?;
        anyhow::ensure!(
            self.store.owner_scope() == owner,
            "experiment stage belongs to another store owner"
        );
        Ok((self.id.clone(), self.binding.experiment_sha256.clone()))
    }
    pub(crate) const fn kind(&self) -> crate::skills::lifecycle::experiment::ExperimentStageKind {
        self.kind
    }
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
        let continuation = super::h2_entry::current_run_continuation();
        match (&plan.spec().controlled_admission, &continuation) {
            (Some(envelope), Some(owner)) => owner.ensure_admission(&envelope.pin, plan, config)?,
            (None, None) => {}
            _ => anyhow::bail!("experiment stage lost or changed its controlled admission owner"),
        }
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
            continuation,
            workspace: config.workspace_dir.clone(),
            case_template_sha256: Self::case_template_digest(config)?,
            configuration: Arc::new(config.clone()),
            accounting: Arc::new(accounting),
            key,
            store,
            id: plan.spec().id.clone(),
            cancel,
            tools: AtomicU64::new(0),
            resources,
            kind: admitted.kind,
            gate_subject: std::sync::OnceLock::new(),
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
                        let status = tokio::task::spawn_blocking(move || {
                            // Source/config observation occurs outside owner locks;
                            // the existing stage watcher owns cancellation.
                            if let Some(continuation)=&read.continuation {
                                continuation.profile()?.verify(&read.configuration)?;
                            }
                            read.ensure_active()
                        }).await;
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
    /// One in-memory expected immutable subject, not a second durable owner.
    pub(crate) fn bind_gate_subject(&self, subject: crate::skills::lifecycle::GateReviewSubject) -> anyhow::Result<()> {
        self.ensure_active()?;
        anyhow::ensure!(
            self.kind == crate::skills::lifecycle::experiment::ExperimentStageKind::GateReview,
            "only GateReview binds a judge subject"
        );
        if let Some(existing) = self.gate_subject.get() {
            anyhow::ensure!(existing == &subject, "GateReview expected subject cannot change");
        } else {
            self.gate_subject
                .set(subject)
                .map_err(|_| anyhow::anyhow!("GateReview expected subject raced"))?;
        }
        Ok(())
    }
    pub(crate) fn gate_subject(&self) -> Option<crate::skills::lifecycle::GateReviewSubject> {
        self.gate_subject.get().cloned()
    }
    /// Actual stage accounting identity, never an input supplied to GateRequest.
    pub(crate) fn gate_record_binding(
        &self,
        owner: &crate::skills::lifecycle::OwnerScope,
    ) -> anyhow::Result<crate::skills::lifecycle::GateExperimentBinding> {
        self.ensure_active()?;
        anyhow::ensure!(
            self.store.owner_scope() == owner,
            "gate runtime owner differs from store"
        );
        anyhow::ensure!(
            self.kind == crate::skills::lifecycle::experiment::ExperimentStageKind::GateReview,
            "only an admitted GateReview stage can bind an experiment gate"
        );
        anyhow::ensure!(
            self.gate_subject.get().is_some(),
            "gate runtime has no expected immutable subject"
        );
        Ok(crate::skills::lifecycle::GateExperimentBinding {
            owner_scope: owner.clone(),
            experiment: self.binding(),
            physical_scope_id: self.scope_id(),
        })
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
        match (&journal.plan.spec().controlled_admission, &self.continuation) {
            (Some(envelope), Some(owner)) => owner.ensure_store_admission(
                &envelope.pin,
                &journal.plan,
                &self.workspace,
                self.store.owner_scope().as_str(),
            )?,
            (None, None) => {}
            _ => anyhow::bail!("experiment stage controlled admission changed"),
        }
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
pub(crate) use files::NativeWrite;
