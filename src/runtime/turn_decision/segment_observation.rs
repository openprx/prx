//! Immediate, confirmed predecessor observations for a serial child task.
//! This is live runtime evidence, not a history read or new execution authority.
use super::{AgentDecisionError, AgentTurnKind, TurnDecisionContext, TurnDecisionService};
use crate::causal_tree::state::{ObservedState, ObservedStep, ObservedStepKind, ObservedStepStatus};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::runtime::turn_source::{OwnedTurnSourceProof, TurnSourceProof};
use anyhow::{Context, ensure};
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_CALLS: usize = 64;
const MAX_PROVIDER_ATTEMPTS: usize = 256;
use crate::memory::ingress_recovery::MAX_ID_BYTES;

/// Fields cannot be assembled by an ingress or deserialized. Construction
/// consumes the terminal owner's private confirmation witness.
pub(in crate::runtime) struct SegmentObservation {
    envelope: RuntimeEnvelope,
    terminal_event_id: Arc<str>,
    terminal_sequence: i64,
    steps: Option<Vec<ObservedStep>>,
    exact_call_observations: Option<Arc<crate::causal_tree::state::exact_calls::ExactCallObservations>>,
}

pub(in crate::runtime) fn validate_envelope(envelope: &RuntimeEnvelope) -> anyhow::Result<()> {
    for value in [
        Some(envelope.workspace_id.as_str()),
        Some(envelope.session_key.as_str()),
        envelope.owner_id.as_deref(),
        envelope.task_id.as_deref(),
        envelope.run_id.as_deref(),
        envelope.source_message_event_id.as_deref(),
        envelope.parent_run_id.as_deref(),
        envelope.config_source_revision.as_deref(),
        envelope.legacy_session_key.as_deref(),
        envelope.topic_id.as_deref(),
        envelope.attempt_id.as_deref(),
        envelope.agent_id.as_deref(),
        envelope.persona_id.as_deref(),
        envelope.channel.as_deref(),
        envelope.sender.as_deref(),
        envelope.recipient.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        ensure!(
            value.len() <= MAX_ID_BYTES,
            "segment observation identity exceeds bounds"
        );
    }
    ensure!(
        !envelope.workspace_id.is_empty()
            && !envelope.session_key.is_empty()
            && envelope.task_id.as_deref().is_some_and(|id| !id.is_empty())
            && envelope
                .source_message_event_id
                .as_deref()
                .is_some_and(|id| !id.is_empty())
            && envelope.run_id.is_some()
            && envelope.owner_id.is_some()
            && envelope.config_generation_id.is_some()
            && envelope.config_source_revision.is_some(),
        "segment observation identity missing"
    );
    Ok(())
}

impl SegmentObservation {
    pub(in crate::runtime) fn confirmed(
        confirmed: crate::runtime::turn_terminal::ConfirmedSegmentTerminal<'_>,
    ) -> anyhow::Result<Self> {
        let (receipt, context, envelope) = confirmed.parts();
        validate_envelope(envelope)?;
        let event = &receipt.terminal_event;
        let trusted = context.engine.state.trusted.parts();
        ensure!(
            event.id > 0
                && event.event_id.len() <= MAX_ID_BYTES
                && !event.event_id.is_empty()
                && event.event_type == "turn.finalized"
                && event.workspace_id == envelope.workspace_id
                && event.owner_id == envelope.owner_id
                && event.session_key.as_deref() == Some(envelope.session_key.as_str())
                && event.run_id == envelope.run_id
                && event.run_id.as_deref() == Some(receipt.terminal_id.as_str())
                && event.causation_event_id == envelope.source_message_event_id
                && event.source.as_str() == envelope.source.as_str()
                && event.parent_run_id == envelope.parent_run_id
                && event.config_generation_id == envelope.config_generation_id
                && event.config_source_revision == envelope.config_source_revision
                && event.visibility == envelope.visibility
                && event.parent_session_key.is_none()
                && event.correlation_id.is_none()
                && event.goal_id.is_none()
                && event.recipient.as_deref() == envelope.recipient.as_deref().filter(|value| !value.is_empty())
                && event.channel == envelope.channel
                && event.sender == envelope.sender
                && event.agent_id == envelope.agent_id
                && event.persona_id == envelope.persona_id
                && event.attempt_id == envelope.attempt_id
                && event.lease_epoch == envelope.lease_epoch
                && matches!(&event.subject, Some(crate::memory::MessageEventSubject::Task(task)) if Some(task) == envelope.task_id.as_ref())
                && trusted.workspace_id.as_ref() == envelope.workspace_id
                && trusted.owner_id.as_deref() == envelope.owner_id.as_deref()
                && trusted.session_key.as_deref() == Some(envelope.session_key.as_str())
                && trusted.run_id.as_deref() == envelope.run_id.as_deref()
                && trusted.source_event_id.as_deref() == envelope.source_message_event_id.as_deref()
                && trusted.task_id.as_deref() == envelope.task_id.as_deref(),
            "confirmed segment receipt differs from its actual source and decision"
        );
        let trace = context.terminal_execution_trace();
        let event_id: Arc<str> = Arc::from(event.event_id.as_str());
        // Incomplete observations poison inheritance, not the already-confirmed
        // terminal. Never substitute the preceding slot or infer a status.
        let steps = if trace
            .exact_call_observations
            .as_ref()
            .is_some_and(|facts| facts.incomplete || facts.validate().is_err())
            || trace.incomplete
            || trace.provider_attempts_incomplete
            || trace.pending_attempts != 0
            || trace.provider_pending != 0
            || trace.unbound_attempts != 0
            || trace.calls.len() > MAX_CALLS
            || trace
                .provider_attempts
                .as_ref()
                .is_some_and(|attempts| attempts.len() > MAX_PROVIDER_ATTEMPTS)
        {
            None
        } else {
            project_trace(&trace, &event_id).ok()
        };
        Ok(Self {
            envelope: envelope.clone(),
            terminal_event_id: event_id,
            terminal_sequence: event.id,
            steps,
            exact_call_observations: trace.exact_call_observations,
        })
    }

    pub(in crate::runtime) fn validate_successor(&self, prior: &Self) -> anyhow::Result<()> {
        ensure!(
            self.terminal_sequence >= prior.terminal_sequence,
            "segment observation confirmation arrived out of order"
        );
        if self.terminal_sequence == prior.terminal_sequence {
            ensure!(
                self.terminal_event_id == prior.terminal_event_id && self.envelope == prior.envelope,
                "segment observation replay changed identity"
            );
        }
        Ok(())
    }

    pub(in crate::runtime) fn matches_next(
        &self,
        proof: &OwnedTurnSourceProof,
        envelope: &RuntimeEnvelope,
    ) -> anyhow::Result<()> {
        validate_envelope(envelope)?;
        let before = &self.envelope;
        ensure!(
            proof.previous_segment_input() == before.source_message_event_id.as_deref()
                && proof.previous_segment_input().is_some()
                && proof
                    .segment_input_sequence()
                    .is_some_and(|sequence| sequence > self.terminal_sequence)
                && proof.source_event_id() == envelope.source_message_event_id.as_deref()
                && proof.workspace_id() == envelope.workspace_id
                && proof.owner_id() == envelope.owner_id.as_deref().context("segment owner missing")?
                && proof.session_key() == envelope.session_key
                && proof.run_id() == envelope.run_id.as_deref().context("segment run missing")?
                && proof.task_id() == envelope.task_id.as_deref()
                && Some(proof.config_generation_id()) == envelope.config_generation_id
                && Some(proof.config_source_revision()) == envelope.config_source_revision.as_deref()
                && before.workspace_id == envelope.workspace_id
                && before.owner_id == envelope.owner_id
                && before.session_key == envelope.session_key
                && before.task_id == envelope.task_id
                && before.parent_run_id == envelope.parent_run_id
                && before.source == envelope.source
                && before.config_generation_id == envelope.config_generation_id
                && before.config_source_revision == envelope.config_source_revision
                && before.visibility == envelope.visibility
                && before.owner_isolated == envelope.owner_isolated
                && before.legacy_session_key == envelope.legacy_session_key
                && before.channel == envelope.channel
                && before.sender == envelope.sender
                && before.recipient == envelope.recipient
                && before.agent_id == envelope.agent_id
                && before.persona_id == envelope.persona_id
                && before.attempt_id == envelope.attempt_id
                && before.lease_epoch == envelope.lease_epoch
                && before.topic_id == envelope.topic_id
                && before.run_id != envelope.run_id
                && before.source_message_event_id != envelope.source_message_event_id,
            "segment observation crosses predecessor, source, task or generation"
        );
        ensure!(self.steps.is_some(), "previous segment observations are incomplete");
        Ok(())
    }

    fn observed(&self, deadline: Instant, cancellation: &CancellationToken) -> anyhow::Result<ObservedState> {
        ensure!(
            !cancellation.is_cancelled() && Instant::now() < deadline,
            "segment observation deadline expired"
        );
        let steps = self
            .steps
            .as_ref()
            .context("previous segment observations are incomplete")?;
        ensure!(
            steps.len() <= MAX_CALLS + MAX_PROVIDER_ATTEMPTS && !self.terminal_event_id.is_empty(),
            "segment observation capacity invalid"
        );
        let observed = ObservedState::from_runtime_records(steps.clone(), Vec::new(), 0)
            .with_exact_call_observations(self.exact_call_observations.clone());
        ensure!(
            !cancellation.is_cancelled() && Instant::now() < deadline,
            "segment observation deadline expired"
        );
        Ok(observed)
    }
}

fn project_trace(
    trace: &crate::tools::capability::execution_trace::ExecutionTraceSnapshot,
    event: &Arc<str>,
) -> anyhow::Result<Vec<ObservedStep>> {
    let providers = trace.provider_attempts.as_deref().unwrap_or_default();
    ensure!(
        trace.provider_requests == 0 || !providers.is_empty(),
        "provider observations missing"
    );
    let mut steps = Vec::with_capacity(trace.calls.len() + providers.len());
    for call in &trace.calls {
        let status = match call.status.as_str() {
            "success" => ObservedStepStatus::Ok,
            "failed" => ObservedStepStatus::Failed,
            "denied" => ObservedStepStatus::Denied,
            "cancelled" => ObservedStepStatus::Cancelled,
            _ => anyhow::bail!("tool observation is not complete"),
        };
        // A capability label is not a current candidate or exact call digest.
        steps.push(ObservedStep::recorded_by_runtime(
            ObservedStepKind::Tool,
            status,
            None,
            Some(Arc::clone(event)),
        ));
    }
    for attempt in providers {
        let status = match attempt.status.as_str() {
            "success" => ObservedStepStatus::Ok,
            "failed" => ObservedStepStatus::Failed,
            "skipped" => continue,
            _ => anyhow::bail!("provider observation is not complete"),
        };
        steps.push(ObservedStep::recorded_by_runtime(
            ObservedStepKind::Provider,
            status,
            None,
            Some(Arc::clone(event)),
        ));
    }
    Ok(steps)
}

impl TurnDecisionService {
    pub(in crate::runtime) async fn decide_agent_segment(
        &self,
        proof: &OwnedTurnSourceProof,
        observation: Option<&SegmentObservation>,
        cancellation: &CancellationToken,
    ) -> Result<Arc<TurnDecisionContext>, AgentDecisionError> {
        let context = self
            .decide_agent_segment_initial_work(proof, observation, cancellation)
            .await?;
        self.record_issued_decision(proof, context, cancellation).await
    }

    #[inline(never)]
    fn decide_agent_segment_initial_work<'a>(
        &'a self,
        proof: &'a OwnedTurnSourceProof,
        observation: Option<&'a SegmentObservation>,
        cancellation: &'a CancellationToken,
    ) -> std::pin::Pin<
        Box<impl std::future::Future<Output = Result<Arc<TurnDecisionContext>, AgentDecisionError>> + Send + 'a>,
    > {
        Box::pin(self.decide_agent_initial_inner(
            proof,
            AgentTurnKind::SingleShot,
            cancellation,
            None,
            observation,
            #[cfg(feature = "llm-router")]
            None,
        ))
    }
}

pub(super) fn initial_observations(
    observation: Option<&SegmentObservation>,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<ObservedState, AgentDecisionError> {
    observation.map_or_else(
        || Ok(ObservedState::from_runtime_records(Vec::new(), Vec::new(), 0)),
        |observation| {
            observation
                .observed(deadline, cancellation)
                .map_err(AgentDecisionError::SegmentObservation)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{SegmentObservation, initial_observations, project_trace};
    use crate::causal_tree::state::{ObservedStepKind, ObservedStepStatus};
    use crate::causal_tree::store::{RecordedOutcomeAttempt, RecordedToolCall};
    use crate::runtime::envelope::RuntimeEnvelope;
    use crate::tools::capability::execution_trace::ExecutionTraceSnapshot;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    #[test]
    fn actual_statuses_preserve_denial_and_do_not_mint_candidates() -> anyhow::Result<()> {
        let trace = ExecutionTraceSnapshot {
            calls: vec![
                RecordedToolCall {
                    capability_id: "native:local:probe".into(),
                    status: "denied".into(),
                },
                RecordedToolCall {
                    capability_id: "native:local:probe".into(),
                    status: "failed".into(),
                },
            ],
            provider_requests: 1,
            provider_attempts: Some(vec![
                RecordedOutcomeAttempt {
                    seq: 1,
                    status: "skipped".into(),
                    error_class: None,
                },
                RecordedOutcomeAttempt {
                    seq: 2,
                    status: "success".into(),
                    error_class: None,
                },
            ]),
            ..ExecutionTraceSnapshot::default()
        };
        let event: Arc<str> = Arc::from("segment-terminal");
        let steps = project_trace(&trace, &event)?;
        assert_eq!(
            steps
                .iter()
                .map(|step| (step.kind(), step.status()))
                .collect::<Vec<_>>(),
            vec![
                (ObservedStepKind::Tool, ObservedStepStatus::Denied),
                (ObservedStepKind::Tool, ObservedStepStatus::Failed),
                (ObservedStepKind::Provider, ObservedStepStatus::Ok),
            ]
        );
        assert!(
            steps
                .iter()
                .all(|step| step.candidate().is_none() && step.event_id().as_deref() == Some("segment-terminal"))
        );
        Ok(())
    }

    #[test]
    fn admitted_identity_bound_is_preserved_for_segment_observations() {
        let mut envelope = RuntimeEnvelope::session_worker(
            "x".repeat(crate::memory::ingress_recovery::MAX_ID_BYTES),
            "session",
            "segment",
        )
        .with_owner_id("owner")
        .with_task_id("task");
        envelope.source_message_event_id = Some("source".into());
        envelope.config_generation_id = Some(1);
        envelope.config_source_revision = Some("revision".into());
        assert!(super::validate_envelope(&envelope).is_ok());
        envelope.workspace_id.push('x');
        assert!(super::validate_envelope(&envelope).is_err());
    }

    #[test]
    fn unknown_status_is_not_guessed_from_text() {
        let trace = ExecutionTraceSnapshot {
            calls: vec![RecordedToolCall {
                capability_id: "native:local:probe".into(),
                status: "Error: failed".into(),
            }],
            ..ExecutionTraceSnapshot::default()
        };
        assert!(project_trace(&trace, &Arc::from("segment-terminal")).is_err());
    }

    #[test]
    fn missing_provider_detail_cannot_be_confirmed() {
        let trace = ExecutionTraceSnapshot {
            provider_requests: 1,
            ..ExecutionTraceSnapshot::default()
        };
        assert!(project_trace(&trace, &Arc::from("segment-terminal")).is_err());
    }

    #[test]
    fn unknown_or_expired_predecessor_cannot_be_projected() {
        let mut observation = SegmentObservation {
            envelope: RuntimeEnvelope::session_worker("workspace", "session", "segment"),
            terminal_event_id: Arc::from("segment-terminal"),
            terminal_sequence: 2,
            steps: None,
            exact_call_observations: None,
        };
        let cancellation = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(initial_observations(Some(&observation), deadline, &cancellation).is_err());
        observation.steps = Some(Vec::new());
        assert!(initial_observations(Some(&observation), Instant::now(), &cancellation).is_err());
        cancellation.cancel();
        assert!(initial_observations(Some(&observation), deadline, &cancellation).is_err());
    }
    #[test]
    fn segment_projection_retains_exact_call_facts_without_candidate_authority() {
        use crate::causal_tree::state::exact_calls::{
            BackendCompletion, ExactCallObservation, ExactCallObservations, LedgerDisposition,
        };
        let facts = Arc::new(ExactCallObservations {
            version: 1,
            incomplete: false,
            calls: vec![ExactCallObservation {
                fingerprint: [7; 32],
                ledger: LedgerDisposition::CachedTerminal,
                reservation_attempt: None,
                backend_started: false,
                completion: BackendCompletion::NotStarted,
            }],
        });
        let observation = SegmentObservation {
            envelope: RuntimeEnvelope::session_worker("prx_workspace", "prx_session", "prx_segment"),
            terminal_event_id: Arc::from("prx_terminal"),
            terminal_sequence: 2,
            steps: Some(Vec::new()),
            exact_call_observations: Some(Arc::clone(&facts)),
        };
        let observed = initial_observations(
            Some(&observation),
            Instant::now() + Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .expect("projection");
        assert_eq!(
            observed.exact_call_observations().map(AsRef::as_ref),
            Some(facts.as_ref())
        );
        assert!(observed.failed_candidates().is_empty());
        assert_eq!(observed.retry_count(), 0, "cached result is not a physical retry");
    }
}
