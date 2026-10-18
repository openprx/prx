//! Source-owned auxiliary accounting and evidence. Physical journal/cost rows
//! are authoritative; this observation never debits aggregate usage again.
use crate::llm::route_decision::{ProviderExecutionOutcome, RouteDecision};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuxiliaryPurpose {
    SignalVision,
    SmartGroupClassifier,
    #[cfg(feature = "llm-router")]
    AutomixPremium,
}
impl AuxiliaryPurpose {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::SignalVision => "signal_vision",
            Self::SmartGroupClassifier => "smart_group_classifier",
            #[cfg(feature = "llm-router")]
            Self::AutomixPremium => "automix_premium",
        }
    }
}

pub(crate) struct AuxiliaryProviderCall {
    route: RouteDecision,
    purpose: AuxiliaryPurpose,
    accounting_scope_id: String,
    state: parking_lot::Mutex<State>,
}
#[derive(Default)]
struct State {
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    outcome: Option<ProviderExecutionOutcome>,
    uncertain: bool,
    completed_reply_sha256: Option<[u8; 32]>,
    served_reply_sha256: Option<[u8; 32]>,
}
impl AuxiliaryProviderCall {
    pub(crate) fn new(route: RouteDecision, accounting_scope_id: String) -> Arc<Self> {
        Self::new_for(AuxiliaryPurpose::SignalVision, route, accounting_scope_id)
    }
    pub(crate) fn new_for(purpose: AuxiliaryPurpose, route: RouteDecision, accounting_scope_id: String) -> Arc<Self> {
        Arc::new(Self {
            route,
            purpose,
            accounting_scope_id,
            state: parking_lot::Mutex::new(State::default()),
        })
    }
    pub(crate) fn begin(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        anyhow::ensure!(state.started_at.is_none(), "auxiliary provider call already started");
        state.started_at = Some(chrono::Utc::now());
        state.uncertain = true;
        Ok(())
    }
    pub(crate) fn complete(&self, trace: &crate::providers::traits::ChatTrace) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        let started_at = state
            .started_at
            .ok_or_else(|| anyhow::anyhow!("auxiliary provider call did not start"))?;
        anyhow::ensure!(state.outcome.is_none(), "auxiliary provider call already completed");
        let mut attempts = trace.attempts.clone();
        for attempt in &mut attempts {
            attempt.error_message = None;
        }
        state.uncertain = !trace.tokens_used.is_reported()
            || trace.tokens_used.prompt_tokens.is_none()
            || trace.tokens_used.completion_tokens.is_none()
            || attempts
                .iter()
                .any(|attempt| attempt.status == crate::llm::route_decision::AttemptStatus::Failed);
        let mut outcome = ProviderExecutionOutcome::from_trace_with_usage(
            &self.route,
            attempts,
            trace.final_provider.clone(),
            trace.final_model.clone(),
            started_at,
            chrono::Utc::now(),
            trace.attempts.len() > 1,
            trace.tokens_used.clone(),
        );
        outcome.redact_provider_labels();
        use sha2::{Digest, Sha256};
        state.completed_reply_sha256 = Some(Sha256::digest(trace.response.text_or_empty().as_bytes()).into());
        state.outcome = Some(outcome);
        Ok(())
    }
    #[cfg(feature = "llm-router")]
    pub(crate) fn mark_served(&self, reply: &str) -> anyhow::Result<()> {
        use crate::llm::route_decision::{AttemptStatus, ExecutionStatus};
        use sha2::{Digest, Sha256};
        let mut state = self.state.lock();
        anyhow::ensure!(
            self.purpose == AuxiliaryPurpose::AutomixPremium,
            "only premium auxiliary reply can be served"
        );
        let outcome = state
            .outcome
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("auxiliary response is not completed"))?;
        let last = outcome
            .attempts
            .iter()
            .rev()
            .find(|attempt| attempt.status != AttemptStatus::Skipped);
        anyhow::ensure!(
            matches!(
                outcome.status,
                ExecutionStatus::Success | ExecutionStatus::FallbackSuccess
            ) && last.is_some_and(|attempt| attempt.status == AttemptStatus::Success
                && attempt.provider == outcome.final_provider
                && attempt.model == outcome.final_model),
            "auxiliary response lacks an actual serving attempt"
        );
        let digest: [u8; 32] = Sha256::digest(reply.as_bytes()).into();
        anyhow::ensure!(
            !reply.trim().is_empty() && state.completed_reply_sha256 == Some(digest),
            "auxiliary served reply differs from actual response"
        );
        anyhow::ensure!(
            state.served_reply_sha256.is_none_or(|prior| prior == digest),
            "auxiliary serving observation already differs"
        );
        state.served_reply_sha256 = Some(digest);
        Ok(())
    }
    pub(crate) fn served_reply_sha256(&self) -> Option<[u8; 32]> {
        self.state.lock().served_reply_sha256
    }
    pub(crate) async fn persist(
        &self,
        fabric: &crate::memory::MemoryFabric,
        config: &crate::config::Config,
        proposal: &mut crate::agent::terminal::TurnTerminalDraft,
    ) -> anyhow::Result<Option<crate::memory::MessageEvent>> {
        let (started_at, outcome, uncertain, served_reply_sha256) = {
            let state = self.state.lock();
            let Some(started_at) = state.started_at else {
                return Ok(None);
            };
            (
                started_at,
                state.outcome.clone(),
                state.uncertain,
                state.served_reply_sha256.map(hex::encode),
            )
        };
        anyhow::ensure!(
            self.accounting_scope_id.len() == 64
                && self.accounting_scope_id.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "auxiliary accounting scope is invalid"
        );
        let purpose = self.purpose.label();
        let idempotency_key = format!("turn:{}:{purpose}", proposal.terminal_id);
        // This is evidence of the logical auxiliary call. The keyed scope ID
        // joins its physical reservation/settlement ledger, restricted to
        // attempt IDs prefixed by this exact purpose plus ':'. This separates
        // decision/router/vision/premium calls sharing a source budget.
        // No aggregate debit is emitted.
        let payload = crate::security::redaction::redact_json_structure(&serde_json::json!({
            "version": 2, "purpose": purpose, "accounting_scope_id": self.accounting_scope_id,
            "started_at": started_at, "provider_outcome": outcome, "usage_uncertain": uncertain,
            "served_reply_sha256": served_reply_sha256,
        }))?;
        let payload = serde_json::to_string(&payload)?;
        let event = fabric
            .record_runtime_event_idempotent(
                proposal.scope.clone(),
                "provider.auxiliary.settled",
                format!("{purpose} auxiliary call"),
                Some(payload.clone()),
                idempotency_key.clone(),
            )
            .await?;
        anyhow::ensure!(
            !config.causal_tree.enabled || event.id > 0,
            "auxiliary call receipt is not durable"
        );
        if event.id > 0 {
            let scope = &proposal.scope;
            anyhow::ensure!(
                event.workspace_id == config.workspace_dir.to_string_lossy()
                    && event.owner_id == scope.owner_id
                    && event.run_id == scope.run_id
                    && event.source == scope.source
                    && event.session_key == scope.session_key
                    && event.parent_session_key == scope.parent_session_key
                    && event.parent_run_id == scope.parent_run_id
                    && event.agent_id == scope.agent_id
                    && event.persona_id == scope.persona_id
                    && event.subject == scope.subject
                    && event.goal_id == scope.goal_id
                    && event.attempt_id == scope.attempt_id
                    && event.lease_epoch == scope.lease_epoch
                    && event.channel == scope.channel
                    && event.sender == scope.sender
                    && event.visibility == scope.visibility
                    && event.correlation_id == scope.correlation_id
                    && event.causation_event_id == scope.causation_event_id
                    && event.config_generation_id == scope.config_generation_id
                    && event.config_source_revision == scope.config_source_revision
                    && event.event_type == "provider.auxiliary.settled"
                    && event.idempotency_key.as_deref() == Some(idempotency_key.as_str())
                    && event.raw_payload_json.as_deref() == Some(payload.as_str()),
                "auxiliary receipt conflicts with its admitted scope"
            );
            let owner = event
                .owner_id
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("auxiliary receipt owner missing"))?;
            let stored = fabric
                .memory()
                .find_owned_message_event(&event.workspace_id, owner, &event.event_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("auxiliary receipt cannot be read back"))?;
            anyhow::ensure!(
                serde_json::to_value(&stored)? == serde_json::to_value(&event)?,
                "auxiliary receipt readback differs"
            );
        }
        proposal.telemetry.summary.push_str(if uncertain {
            "; auxiliary physical usage remains uncertain; see accounting scope"
        } else {
            "; auxiliary physical usage accounted separately; see accounting scope"
        });
        Ok((event.id > 0).then_some(event))
    }
}

/// Issued only by the source-bound decision service. The descriptor and provider
/// are captured from the same actual constructor/registered instance.
pub(crate) struct AuxiliaryAccounting {
    selected: crate::providers::catalog::ResolvedModel,
    requested: (String, String),
    provider: Arc<dyn crate::providers::Provider>,
    targets: crate::providers::AccountingTargets,
    key: Arc<crate::decision::DecisionKey>,
    scope: Arc<super::decision_accounting::AccountingScope>,
    purpose: AuxiliaryPurpose,
    execution_trace: crate::tools::capability::execution_trace::ExecutionTrace,
}
impl AuxiliaryAccounting {
    pub(super) fn new(
        selected: crate::providers::catalog::ResolvedModel,
        requested: (String, String),
        provider: Arc<dyn crate::providers::Provider>,
        key: Arc<crate::decision::DecisionKey>,
        scope: Arc<super::decision_accounting::AccountingScope>,
        purpose: AuxiliaryPurpose,
        execution_trace: crate::tools::capability::execution_trace::ExecutionTrace,
    ) -> anyhow::Result<Arc<Self>> {
        let targets = provider
            .accounting_targets(selected.model())
            .ok_or_else(|| anyhow::anyhow!("auxiliary provider accounting target is unsupported"))?;
        Ok(Arc::new(Self {
            selected,
            requested,
            provider,
            targets,
            key,
            scope,
            purpose,
            execution_trace,
        }))
    }
    pub(crate) const fn selected_model(&self) -> &crate::providers::catalog::ResolvedModel {
        &self.selected
    }
    pub(crate) fn requested_selection(&self) -> (&str, &str) {
        (&self.requested.0, &self.requested.1)
    }
    pub(crate) fn provider(&self) -> Arc<dyn crate::providers::Provider> {
        Arc::clone(&self.provider)
    }
    pub(crate) fn scope_id(&self) -> String {
        self.scope.scope_id()
    }
    pub(crate) fn deadline(&self) -> std::time::Instant {
        self.scope.deadline()
    }
    pub(crate) fn begin_provider_observation(
        &self,
    ) -> anyhow::Result<crate::tools::capability::execution_trace::PendingProvider> {
        self.execution_trace.begin_provider()
    }
    pub(crate) fn authorize_silence(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.purpose == AuxiliaryPurpose::SmartGroupClassifier,
            "only the smart-group classifier can authorize auxiliary silence"
        );
        self.execution_trace.record_authorized_silence()
    }
}
impl crate::providers::attempt_accounting::sealed::AccountingOwner for AuxiliaryAccounting {}
#[async_trait::async_trait]
impl crate::providers::attempt_accounting::AttemptAccounting for AuxiliaryAccounting {
    fn deadline(&self) -> std::time::Instant {
        self.scope.deadline()
    }
    async fn begin(
        &self,
        request: crate::providers::attempt_accounting::PhysicalRequest<'_>,
    ) -> anyhow::Result<Box<dyn crate::providers::attempt_accounting::AttemptReceipt>> {
        use crate::providers::attempt_accounting::{AccountingRefusal, PhysicalKind};
        anyhow::ensure!(request.kind() == PhysicalKind::Chat, AccountingRefusal);
        let (provider, model) = self
            .targets
            .match_request(&request, self.selected.provider_label())
            .ok_or(AccountingRefusal)?;
        let credential_present = [u8::from(request.credential().is_some())];
        let binding = self.key.hmac(
            crate::decision::KeyDomain::AttemptAccounting,
            &[
                b"auxiliary-physical-chat-v1",
                self.purpose.label().as_bytes(),
                provider.as_bytes(),
                model.as_bytes(),
                request.endpoint().as_bytes(),
                self.requested_selection().0.as_bytes(),
                self.requested_selection().1.as_bytes(),
                &credential_present,
                request.credential().unwrap_or("").as_bytes(),
            ],
        );
        let receipt = self
            .scope
            .begin_physical(super::decision_accounting::PhysicalEstimate {
                purpose: self.purpose.label(),
                provider,
                model,
                body: request.body(),
                transport_binding: &binding,
                request_id: self.purpose.label(),
                deadline: self.scope.deadline(),
                remaining_attempts: 1,
            })
            .await?;
        Ok(Box::new(AuxiliaryReceipt(receipt)))
    }
}
struct AuxiliaryReceipt(super::decision_accounting::OwnedReceipt);
impl crate::providers::attempt_accounting::sealed::Receipt for AuxiliaryReceipt {}
#[async_trait::async_trait]
impl crate::providers::attempt_accounting::AttemptReceipt for AuxiliaryReceipt {
    fn deadline(&self) -> std::time::Instant {
        crate::decision::budget::DecisionAttemptReceipt::deadline(&self.0)
    }
    async fn settle(self: Box<Self>, usage: crate::llm::route_decision::TokenUsage) -> anyhow::Result<()> {
        Box::new(self.0).settle_provider_usage(usage).map_err(Into::into)
    }
}
