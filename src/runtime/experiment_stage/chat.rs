//! Prospective physical accounting for the solver's ordinary provider calls.
//! The turn engine retains all of its existing source/model admission checks.
use super::super::decision_accounting::PhysicalEstimate;
use super::{Arc, ExperimentStageRuntime, Instant, current};
use crate::providers::traits::ChatTrace;
use crate::providers::{
    ChatRequest, Provider,
    attempt_accounting::{self, AttemptAccounting, AttemptReceipt, PhysicalRequest},
};

pub(crate) async fn chat_traced(
    provider: &dyn Provider,
    label: &str,
    source_run: &str,
    request: ChatRequest<'_>,
    model: &str,
    temperature: f64,
) -> anyhow::Result<ChatTrace> {
    let Some(stage) = current() else {
        return provider.chat_traced(request, model, temperature).await;
    };
    stage.ensure_active()?;
    anyhow::ensure!(
        !source_run.is_empty(),
        "experiment solver has no source-owned run identity"
    );
    let targets = provider
        .accounting_targets(model)
        .ok_or_else(|| anyhow::anyhow!("experiment solver transport has no physical accounting binding"))?;
    let accounting = Arc::new(SolverAccounting {
        stage,
        targets,
        label: label.into(),
        request_id: source_run.into(),
        parameters: crate::providers::parameter_capture::current(),
    });
    attempt_accounting::chat_traced(provider, request, model, temperature, accounting).await
}
struct SolverAccounting {
    stage: Arc<ExperimentStageRuntime>,
    targets: crate::providers::AccountingTargets,
    label: String,
    request_id: String,
    parameters: crate::providers::parameter_capture::Capture,
}
impl attempt_accounting::sealed::AccountingOwner for SolverAccounting {}
#[async_trait::async_trait]
impl AttemptAccounting for SolverAccounting {
    fn deadline(&self) -> Instant {
        self.stage.accounting.deadline()
    }
    fn parameter_capture(&self) -> Option<crate::providers::parameter_capture::Capture> {
        Some(self.parameters.clone())
    }
    async fn begin(&self, request: PhysicalRequest<'_>) -> anyhow::Result<Box<dyn AttemptReceipt>> {
        use attempt_accounting::{AccountingRefusal, PhysicalKind};
        self.stage.ensure_active()?;
        anyhow::ensure!(request.kind() == PhysicalKind::Chat, AccountingRefusal);
        let (provider, model) = self
            .targets
            .match_request(&request, &self.label)
            .ok_or(AccountingRefusal)?;
        let binding = self.stage.key.hmac(
            crate::decision::KeyDomain::AttemptAccounting,
            &[
                b"experiment-solver-physical-chat-v1",
                self.stage.binding.experiment_sha256.as_bytes(),
                self.stage.binding.stage.as_bytes(),
                self.request_id.as_bytes(),
                provider.as_bytes(),
                model.as_bytes(),
                request.endpoint().as_bytes(),
                &[u8::from(request.credential().is_some())],
                request.credential().unwrap_or("").as_bytes(),
            ],
        );
        let receipt = self
            .stage
            .accounting
            .begin_physical(PhysicalEstimate {
                purpose: "experiment_solver",
                provider,
                model,
                body: request.body(),
                transport_binding: &binding,
                request_id: &self.request_id,
                deadline: self.deadline(),
                remaining_attempts: 1,
            })
            .await?;
        Ok(receipt.into_provider_receipt())
    }
}
