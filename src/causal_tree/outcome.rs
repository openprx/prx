//! Pure construction of bounded decision records. A successful turn queues
//! the complete chain separately; this module performs no I/O.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use super::branch::{CandidateId, CapabilityTarget};
use super::candidate_generator::CandidateSet;
use super::decision_engine::{EngineDecision, FallbackCause, PredictionAuditAnswer};
use super::decision_score::SCORER_VERSION;
use super::decision_select::Selection;
use super::gates::{GateOutcome, GateReport};
use super::ids::DecisionId;
use super::state::HopLineage;
use super::store::{
    DecisionRecord, DecisionStoreWriter, DecisionView, Enqueue, OutcomeClassification, OutcomeCompleteness,
    OutcomeRecord, RecordLineage, RecordedAttempt, RecordedCandidate, RecordedDistributions, RecordedOutcomeAttempt,
    RecordedShadow, same_turn_chain,
};
use crate::agent::terminal::{TurnTerminalReceipt, TurnTerminalStatus};
use crate::cost::types::CostSettlement;
use crate::decision::factory::{AttemptOutcome, FallbackReason, ServedBy};
use crate::decision::{DecisionAnswer, DecisionKey, GlobalQuestion, KeyDomain, PROTOCOL_VERSION, QuestionId};
use crate::llm::route_decision::{AttemptStatus, ExecutionStatus, MeteredTokenUsageRecord, ProviderExecutionOutcome};
use crate::runtime::turn_decision::TurnDecisionContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RecordBuildError {
    #[error("decision hop chain does not match its runtime lineage")]
    Chain,
    #[error("decision record facts disagree with the sealed request")]
    Binding,
    #[error("decision record has an unrepresentable number")]
    Number,
}

pub(crate) struct DecisionRecordBuilder;

impl DecisionRecordBuilder {
    /// Build every hop before any queue admission. A failed or cancelled
    /// service call has no context and cannot reach this method.
    pub(crate) fn build_enabled_chain(
        context: &TurnDecisionContext,
        key: &DecisionKey,
        enabled_and_recordable: bool,
        record_redacted_projection: bool,
    ) -> Result<Vec<DecisionRecord>, RecordBuildError> {
        if !enabled_and_recordable {
            return Err(RecordBuildError::Binding);
        }
        let hops: Vec<_> = context.audit_hops().collect();
        if hops.is_empty() || hops.len() > 5 || hops.len() != context.hop_ids().len() {
            return Err(RecordBuildError::Chain);
        }
        let mut records = Vec::with_capacity(hops.len());
        for (index, (hop, expected_id)) in hops.iter().zip(context.hop_ids()).enumerate() {
            if hop.decision_id != *expected_id {
                return Err(RecordBuildError::Chain);
            }
            let prior = match (index, hop.state.lineage) {
                (0, HopLineage::First) => None,
                (position, HopLineage::After { hop, prior_decision_id })
                    if usize::from(hop.get()) == position
                        && records.last().is_some_and(|record: &DecisionRecord| {
                            record.decision_id.as_uuid() == prior_decision_id
                        }) =>
                {
                    Some(records.last().ok_or(RecordBuildError::Chain)?.decision_id)
                }
                _ => return Err(RecordBuildError::Chain),
            };
            records.push(Self::build_hop(
                hop,
                key,
                prior,
                enabled_and_recordable,
                record_redacted_projection,
            )?);
        }
        if !same_turn_chain(&records) {
            return Err(RecordBuildError::Chain);
        }
        Ok(records)
    }

    /// Persist the actual selected first Router hop before its read begins.
    /// Rebuilding it in the eventual full chain uses the same immutable hop,
    /// IDs and timestamps, so the store requires an identical replay hash.
    #[cfg(feature = "llm-router")]
    pub(crate) fn build_router_history_read(
        hop: &EngineDecision,
        key: &DecisionKey,
        record_redacted_projection: bool,
    ) -> Result<DecisionRecord, RecordBuildError> {
        if hop.state.lineage != HopLineage::First || hop.state.trusted.router_history().is_none() {
            return Err(RecordBuildError::Binding);
        }
        let selected = match &hop.selection {
            crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. }
            | crate::causal_tree::decision_select::Selection::Recover { candidate_id, .. } => candidate_id,
            _ => return Err(RecordBuildError::Binding),
        };
        let candidate = hop
            .state
            .derived
            .candidates()
            .iter()
            .find(|candidate| candidate.candidate.candidate_id() == selected)
            .map(|candidate| &candidate.candidate)
            .ok_or(RecordBuildError::Binding)?;
        if candidate.action() != crate::causal_tree::branch::CausalActionKind::Retrieve
            || candidate.target() != crate::causal_tree::branch::CapabilityTarget::Memory
            || candidate.required_capabilities().len() != 1
            || !matches!(
                hop.pre_commit.outcomes.get(selected),
                Some(crate::causal_tree::gates::GateOutcome::Passed)
            )
        {
            return Err(RecordBuildError::Binding);
        }
        Self::build_hop(hop, key, None, true, record_redacted_projection)
    }

    fn build_hop(
        hop: &EngineDecision,
        key: &DecisionKey,
        prior_decision_id: Option<DecisionId>,
        enabled_and_recordable: bool,
        record_redacted_projection: bool,
    ) -> Result<DecisionRecord, RecordBuildError> {
        let trusted = hop.state.trusted.parts();
        let state_fingerprint = hop.state.material_fingerprint(key);
        let catalog_generation = trusted.catalog_generation.to_hex();
        let projection = match (&hop.audit_trace, record_redacted_projection) {
            (Some(trace), enabled) => {
                if trace.state_fingerprint != state_fingerprint
                    || trace.policy_version != trusted.policy_version.as_ref()
                    || trace.catalog_generation != catalog_generation
                    || trace.redacted_projection.is_some() != enabled
                    || !hop.request_compiled
                    || hop.prediction.is_none()
                {
                    return Err(RecordBuildError::Binding);
                }
                trace.redacted_projection.clone()
            }
            (None, _) if hop.prediction.is_none() => None,
            _ => return Err(RecordBuildError::Binding),
        };
        let (backend_kind, backend_locality, configured_model, backend_endpoint, claimed_model, provider_request_id) =
            hop.prediction.as_ref().map_or_else(
                || {
                    // A compiled request may fail without a validated response.
                    (
                        "none".to_owned(),
                        "none".to_owned(),
                        "none".to_owned(),
                        None,
                        None,
                        None,
                    )
                },
                |prediction| {
                    let identity = prediction.served.identity();
                    (
                        identity.kind.as_str().to_owned(),
                        prediction.served_locality.as_str().to_owned(),
                        identity.configured_model.to_string(),
                        prediction.served_endpoint.as_ref().map(ToString::to_string),
                        Some(prediction.served.claimed_model().to_owned()),
                        prediction.served.provider_request_id().map(str::to_owned),
                    )
                },
            );
        let candidates = match &hop.candidates {
            CandidateSet::Legal { candidates, .. } => candidates,
            CandidateSet::NoLegalPath { terminals, .. } => terminals,
        };
        let candidates = candidates
            .iter()
            .map(|candidate| {
                let propensity = hop.exploration_propensities.get(candidate.candidate_id()).copied();
                if propensity.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
                    return Err(RecordBuildError::Number);
                }
                Ok(RecordedCandidate {
                    id: candidate.candidate_id().as_str().to_owned(),
                    action: candidate.action().as_str().to_owned(),
                    target: candidate.target().as_str().to_owned(),
                    propensity,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (selected_candidate_id, directive_kind, abstention_reason, propensity) = selection_fields(hop)?;
        let (distributions, features) = audit_fields(hop)?;
        let builtin_shadow = shadow_fields(hop);
        let first_gates = match &hop.candidates {
            CandidateSet::Legal { report, .. } | CandidateSet::NoLegalPath { report, .. } => report,
        };
        let mut gate_values = serde_json::Map::new();
        gate_values.insert("pre_prediction".to_owned(), gate_fields(first_gates, key));
        gate_values.insert("post_rehearsal".to_owned(), gate_fields(&hop.post_rehearsal, key));
        gate_values.insert("pre_commit".to_owned(), gate_fields(&hop.pre_commit, key));
        let mut gates = Value::Object(gate_values);
        if let Selection::CollectApprovalArguments(proof) = &hop.selection {
            if proof.state_fingerprint() != state_fingerprint
                || hop.request_compiled
                || hop.prediction.is_some()
                || !hop.scores.is_empty()
                || hop.outgoing_child.is_some()
            {
                return Err(RecordBuildError::Binding);
            }
            gates.as_object_mut().ok_or(RecordBuildError::Binding)?.insert(
                "approval_collection".to_owned(),
                json!({ "version": 1, "state_fingerprint": proof.state_fingerprint(),
                    "capability": { "id": proof.capability().id.as_str(),
                        "schema": hex::encode(proof.capability().fingerprint.0),
                        "generation": proof.capability().generation.to_hex() }, "adapter": match proof.adapter() {
                        super::approval_collection::ApprovalReviewAdapter::ChatOwnerReview => "chat_owner_review",
                        super::approval_collection::ApprovalReviewAdapter::ConsoleOwnerReview => "console_owner_review",
                    } }),
            );
        }
        let attempts = hop.prediction.as_ref().map_or_else(Vec::new, |prediction| {
            prediction
                .attempts
                .iter()
                .map(|attempt| {
                    let (outcome, error_class) = match &attempt.outcome {
                        AttemptOutcome::Served => ("served", None),
                        AttemptOutcome::Cached => ("cached", None),
                        AttemptOutcome::Failed(error) => ("failed", Some(error.class().to_owned())),
                        AttemptOutcome::Skipped(error) => ("skipped", Some(error.class().to_owned())),
                    };
                    RecordedAttempt {
                        backend: attempt.identity.kind.as_str().to_owned(),
                        outcome: outcome.to_owned(),
                        error_class,
                        latency_ms: u64::try_from(attempt.latency.as_millis()).unwrap_or(u64::MAX),
                    }
                })
                .collect()
        });
        let prediction = hop.prediction.as_ref();
        let usage = prediction.and_then(|prediction| {
            let usage = prediction.served.usage();
            (usage.input_tokens.is_some() || usage.output_tokens.is_some() || usage.cost_micro_usd.is_some()).then(
                || {
                    json!({
                        "input_tokens": usage.input_tokens,
                        "output_tokens": usage.output_tokens,
                        "cost_micro_usd": usage.cost_micro_usd,
                    })
                },
            )
        });
        let created_at_ms = i64::try_from(hop.state.built_at_ms).map_err(|_| RecordBuildError::Number)?;
        Ok(DecisionRecord {
            decision_id: hop.decision_id,
            created_at_ms,
            lineage: RecordLineage {
                workspace_id: Arc::clone(&trusted.workspace_id),
                prior_decision_id,
                owner_id: trusted.owner_id.clone(),
                session_key: trusted.session_key.clone(),
                run_id: trusted.run_id.clone(),
                task_id: trusted.task_id.clone(),
                source_event_id: trusted.source_event_id.clone(),
                causation_id: trusted.causation_event_id.clone(),
                channel: trusted.channel.clone(),
            },
            enabled_snapshot: enabled_and_recordable,
            state_fingerprint,
            redacted_projection: projection,
            protocol_version: if hop.request_compiled {
                u32::from(PROTOCOL_VERSION)
            } else {
                0
            },
            policy_version: trusted.policy_version.to_string(),
            catalog_generation,
            scorer_version: SCORER_VERSION.to_owned(),
            backend_kind,
            backend_locality,
            configured_model,
            backend_endpoint,
            claimed_model,
            provider_request_id,
            candidates,
            gates,
            distributions,
            builtin_shadow,
            features,
            selected_candidate_id,
            directive_kind,
            fallback_order: prediction.map_or_else(Vec::new, |prediction| {
                prediction
                    .fallback_order
                    .iter()
                    .map(|identity| identity.kind.as_str().to_owned())
                    .collect()
            }),
            abstention_reason,
            propensity,
            explored: !hop.exploration_propensities.is_empty(),
            latency_ms: hop.elapsed_ms,
            usage,
            attempts,
            // A local terminal is not a backend failure. Attempt classes
            // remain in `attempts`; no unattributed failure is invented.
            error_class: None,
            cache_hit: prediction.is_some_and(|prediction| prediction.served_by == ServedBy::Cache),
            response_sha256: prediction.and_then(|prediction| prediction.served.response_sha256().map(hex::encode)),
            legacy_v1: false,
        })
    }
}

fn selection_fields(hop: &EngineDecision) -> Result<(Option<String>, String, Option<String>, f64), RecordBuildError> {
    let fallback = hop.fallback_cause.map(fallback_code);
    let (selected, action, reason, propensity) = match &hop.selection {
        Selection::CollectApprovalArguments(_) => (None, "collect_approval_arguments", None, 0.0),
        Selection::Commit {
            candidate_id,
            propensity,
        } => (
            Some(candidate_id.clone()),
            candidate_id.action().as_str(),
            fallback,
            *propensity,
        ),
        Selection::Recover {
            candidate_id,
            reason_code,
        } => (
            Some(candidate_id.clone()),
            candidate_id.action().as_str(),
            Some(*reason_code),
            1.0,
        ),
        Selection::RequestApproval {
            terminal_id,
            reason_code,
            ..
        } => (Some(terminal_id.clone()), "request_approval", Some(*reason_code), 1.0),
        Selection::Abstain { action, reason_code } => {
            let terminal_id = CandidateId::new(*action, CapabilityTarget::None, None);
            let admitted = match &hop.candidates {
                CandidateSet::Legal { candidates, report, .. } => {
                    candidates
                        .iter()
                        .any(|candidate| candidate.candidate_id() == &terminal_id)
                        && matches!(report.outcomes.get(&terminal_id), Some(GateOutcome::Passed))
                }
                CandidateSet::NoLegalPath { terminals, report, .. } => {
                    terminals
                        .iter()
                        .any(|candidate| candidate.candidate_id() == &terminal_id)
                        && matches!(report.outcomes.get(&terminal_id), Some(GateOutcome::Passed))
                }
            };
            if !admitted || !matches!(hop.pre_commit.outcomes.get(&terminal_id), Some(GateOutcome::Passed)) {
                return Err(RecordBuildError::Binding);
            }
            (Some(terminal_id), action.as_str(), fallback.or(Some(*reason_code)), 1.0)
        }
    };
    if !propensity.is_finite() || !(0.0..=1.0).contains(&propensity) {
        return Err(RecordBuildError::Number);
    }
    Ok((
        selected.map(|id| id.as_str().to_owned()),
        action.to_owned(),
        reason.map(str::to_owned),
        propensity,
    ))
}

const fn fallback_code(cause: FallbackCause) -> &'static str {
    match cause {
        FallbackCause::NoLegalPath => "no_legal_path",
        FallbackCause::ExpiredBeforePrediction => "expired_before_prediction",
        FallbackCause::DecisionFailed => "decision_failed",
        FallbackCause::DeadlineExceeded => "deadline_exceeded",
        FallbackCause::HopStopped => "hop_stopped",
    }
}

const fn factory_reason(reason: FallbackReason) -> &'static str {
    match reason {
        FallbackReason::DeadlineExhausted => "deadline_exhausted",
        FallbackReason::AllFailed => "all_failed",
        FallbackReason::Disabled => "disabled",
        FallbackReason::BudgetUnproven => "budget_unproven",
    }
}

fn gate_fields(report: &GateReport, key: &DecisionKey) -> Value {
    Value::Object(
        report
            .outcomes
            .iter()
            .map(|(id, outcome)| {
                let status = match outcome {
                    GateOutcome::Passed => json!({"status":"passed"}),
                    GateOutcome::Blocked { gate, reason_code } => {
                        json!({"status":"blocked", "gate":gate, "reason":reason_code})
                    }
                    GateOutcome::ApprovalOnly { op_ids } => json!({
                        "status":"approval_only",
                        "op_digests": op_ids.iter().map(|op| hex::encode(key.hmac(
                            KeyDomain::StateFingerprint,
                            &[b"record-approval-op", op.as_str().as_bytes()],
                        ))).collect::<Vec<_>>(),
                    }),
                    GateOutcome::ApprovalDeferred => json!({"status":"approval_deferred"}),
                };
                (id.as_str().to_owned(), status)
            })
            .collect(),
    )
}

fn audit_fields(hop: &EngineDecision) -> Result<(RecordedDistributions, Value), RecordBuildError> {
    let mut distributions = RecordedDistributions::default();
    let mut answer_fields = serde_json::Map::new();
    let mut omitted = BTreeMap::new();
    let mut forced = BTreeMap::new();
    if let Some(trace) = &hop.audit_trace {
        for (id, answer) in &trace.answers {
            let value = match answer {
                PredictionAuditAnswer::Choice {
                    chosen,
                    probabilities,
                    confidence,
                } => {
                    check_numbers(probabilities.values().copied().chain(std::iter::once(*confidence)))?;
                    if id == GlobalQuestion::NextAction.as_str() {
                        distributions.next_action = probabilities.clone();
                    } else if id == GlobalQuestion::CapabilityTarget.as_str() {
                        distributions.capability_target = probabilities.clone();
                    }
                    json!({"kind":"choice", "chosen":chosen, "probabilities":probabilities, "confidence":confidence})
                }
                PredictionAuditAnswer::Score {
                    distribution,
                    expected,
                    confidence,
                } => {
                    check_numbers(distribution.iter().copied().chain([*expected, *confidence]))?;
                    json!({"kind":"score", "distribution":distribution, "expected":expected, "confidence":confidence})
                }
                PredictionAuditAnswer::Noul { probability_true } => {
                    check_numbers([*probability_true])?;
                    distributions.features.insert(id.clone(), *probability_true);
                    json!({"kind":"noul", "probability_true":probability_true})
                }
            };
            answer_fields.insert(id.clone(), value);
        }
        omitted = trace.omitted_questions.clone();
        forced = trace.forced_choices.clone();
        for question in forced.keys() {
            if question != GlobalQuestion::NextAction.as_str() && question != GlobalQuestion::CapabilityTarget.as_str()
            {
                return Err(RecordBuildError::Binding);
            }
            if (question == GlobalQuestion::NextAction.as_str() && !distributions.next_action.is_empty())
                || (question == GlobalQuestion::CapabilityTarget.as_str()
                    && !distributions.capability_target.is_empty())
            {
                return Err(RecordBuildError::Binding);
            }
        }
    }
    let mut scores = BTreeMap::new();
    for (id, score) in &hop.scores {
        check_numbers([score.semantic_utility, score.execution_utility])?;
        scores.insert(
            id.as_str().to_owned(),
            json!({
                "semantic_utility": score.semantic_utility,
                "execution_utility": score.execution_utility,
            }),
        );
    }
    Ok((
        distributions,
        json!({
            "answers":answer_fields,
            "omitted_questions":omitted,
            "forced_choices":forced,
            "scores":scores,
            "fallback_cause":hop.fallback_cause.map(fallback_code),
            "factory_reason":hop.prediction.as_ref().and_then(|prediction| prediction.reason.map(factory_reason)),
        }),
    ))
}

fn check_numbers(values: impl IntoIterator<Item = f64>) -> Result<(), RecordBuildError> {
    if values.into_iter().all(f64::is_finite) {
        Ok(())
    } else {
        Err(RecordBuildError::Number)
    }
}

fn shadow_fields(hop: &EngineDecision) -> RecordedShadow {
    let Some(prediction) = &hop.prediction else {
        return RecordedShadow::default();
    };
    let selected = |question| {
        let id = QuestionId::global(question);
        match prediction.builtin_shadow.answer(&id) {
            Some(DecisionAnswer::Choice(answer)) => Some(answer.chosen.as_str().to_owned()),
            _ => None,
        }
    };
    RecordedShadow {
        next_action: selected(GlobalQuestion::NextAction),
        capability_target: selected(GlobalQuestion::CapabilityTarget),
    }
}

/// Only facts recoverable from a committed `turn.finalized` marker and its
/// matching receipt. The marker may be read again after a process restart.
/// Raw provider labels, errors, reply text and tool payloads never leave this
/// constructor as outcome fields.
pub(crate) struct TerminalFacts {
    collected_pending: bool,
    resume_claimed: bool,
    terminal_id: String,
    status: TurnTerminalStatus,
    provider_outcome: Option<ProviderExecutionOutcome>,
    usage: Option<MeteredTokenUsageRecord>,
    cost: Option<CostSettlement>,
    latency_ms: u64,
    execution_trace: Option<crate::tools::capability::execution_trace::ExecutionTraceSnapshot>,
}

#[derive(Deserialize)]
struct TerminalMarker {
    terminal_id: String,
    /// CDP-23 writes this typed link into the durable terminal marker before
    /// any correlation is enabled. Legacy or unrelated markers fail closed.
    decision_id: Option<String>,
    status: TurnTerminalStatus,
    provider_outcome: Option<ProviderExecutionOutcome>,
    usage_settlement: Option<MeteredTokenUsageRecord>,
    cost_settlement: Option<CostSettlement>,
    telemetry: TerminalTimes,
    execution_trace: Option<crate::tools::capability::execution_trace::ExecutionTraceSnapshot>,
}

#[derive(Deserialize)]
struct TerminalTimes {
    started_at: chrono::DateTime<chrono::Utc>,
    finished_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum OutcomeCorrelationError {
    #[error("terminal marker and decision lineage do not match")]
    Lineage,
    #[error("terminal marker facts disagree with its receipt")]
    Marker,
    #[error("terminal status cannot be classified from the available execution facts")]
    Unsupported,
    #[error("terminal usage or timing cannot be represented")]
    Number,
}

impl TerminalFacts {
    pub(crate) const fn with_verified_resume(
        mut self,
        replay: &crate::agent::terminal::VerifiedTerminalReplay,
    ) -> Self {
        self.resume_claimed = replay.resume_claimed();
        self.collected_pending = replay.collected_pending();
        self
    }

    /// A direct approved call has no main-provider request. Its successful
    /// trace counts only when durable replay also authenticates consumption.
    pub(crate) fn has_completed_resume_evidence(&self, run_capabilities: &[String]) -> bool {
        self.resume_claimed
            && self.status == TurnTerminalStatus::Completed
            && self.provider_outcome.is_none()
            && self.usage.is_none()
            && self.cost.is_none()
            && run_capabilities.len() == 1
            && self.execution_trace.as_ref().is_some_and(|trace| {
                trace.provider_requests == 0
                    && trace.provider_pending == 0
                    && !trace.provider_attempts_incomplete
                    && trace.provider_attempts.as_ref().is_some_and(Vec::is_empty)
                    && !trace.incomplete
                    && trace.started_attempts == 1
                    && trace.successful_attempts == 1
                    && trace.selected_successful_attempts == 1
                    && trace.pending_attempts == 0
                    && trace.unbound_attempts == 0
                    && trace.failed_attempts == 0
                    && trace.cancelled_attempts == 0
                    && trace.denied_attempts == 0
                    && trace.awaiting_approval_attempts == 0
                    && matches!((trace.calls.as_slice(), run_capabilities), ([call], [capability])
                        if call.capability_id == *capability && call.status == "success")
            })
    }
    /// Conservative replay evidence for a completed Run. A signed terminal
    /// alone does not prove that constrained execution was consumed.
    pub(crate) fn has_completed_run_evidence(&self, decision: &DecisionView, run_capabilities: &[String]) -> bool {
        if self.collected_pending
            || self.status != TurnTerminalStatus::Completed
            || !matches!(
                self.provider_outcome.as_ref().map(|outcome| &outcome.status),
                Some(ExecutionStatus::Success | ExecutionStatus::FallbackSuccess)
            )
        {
            return false;
        }
        let Some(trace) = self.execution_trace.as_ref() else {
            return false;
        };
        if trace.provider_requests == 0
            || trace.provider_pending != 0
            || trace.provider_attempts_incomplete
            || trace.pending_attempts != 0
            || trace.unbound_attempts != 0
            || trace.failed_attempts != 0
            || trace.cancelled_attempts != 0
            || trace.denied_attempts != 0
            || trace.awaiting_approval_attempts != 0
            || trace.successful_attempts != trace.started_attempts
        {
            return false;
        }
        match decision.selected().map(|candidate| candidate.action.as_str()) {
            Some("answer") => trace.started_attempts == 0,
            Some("use_tool" | "delegate") => {
                trace.selected_successful_attempts > 0
                    && !trace.incomplete
                    && trace
                        .calls
                        .iter()
                        .any(|call| run_capabilities.contains(&call.capability_id))
            }
            _ => false,
        }
    }

    pub(crate) fn is_completed_local_close(&self) -> bool {
        self.status == TurnTerminalStatus::Completed
            && self.provider_outcome.is_none()
            && self.usage.is_none()
            && self.cost.is_none()
    }

    /// Reconstruct the exact bounded outcome facts after a process restart.
    /// The caller must obtain `event` from the durable memory store under the
    /// terminal idempotency key and `decision` from the matching owner scope.
    pub(crate) fn from_persisted_marker(
        event: &crate::memory::MessageEvent,
        decision: &DecisionView,
        owner_id: &str,
        workspace_id: &str,
    ) -> Result<Self, OutcomeCorrelationError> {
        if !decision.enabled_snapshot
            || decision.legacy_v1
            || !decision.has_owner
            || event.id <= 0
            || event.role != "event"
            || event.event_type != "turn.finalized"
            || crate::runtime::turn_source::ingress_event_surface(
                &event.source,
                event.channel.as_deref(),
                &event.visibility,
            )
            .is_none()
            || event.workspace_id != workspace_id
            || event.owner_id.as_deref() != Some(owner_id)
            || event.session_key != decision.session_key
            || event.run_id != decision.run_id
            || crate::runtime::turn_source::subject_task_id(event.subject.as_ref()) != decision.task_id.as_deref()
            || decision.channel.is_some()
        {
            return Err(OutcomeCorrelationError::Lineage);
        }
        let marker: TerminalMarker = serde_json::from_str(
            event
                .raw_payload_json
                .as_deref()
                .ok_or(OutcomeCorrelationError::Marker)?,
        )
        .map_err(|_| OutcomeCorrelationError::Marker)?;
        if marker.decision_id.as_deref() != Some(decision.decision_id.to_string().as_str())
            || event.idempotency_key.as_deref() != Some(format!("turn:{}:final", marker.terminal_id).as_str())
            || marker
                .usage_settlement
                .as_ref()
                .is_some_and(|usage| usage.settlement_id.as_deref() != Some(marker.terminal_id.as_str()))
        {
            return Err(OutcomeCorrelationError::Marker);
        }
        let elapsed = marker
            .telemetry
            .finished_at
            .signed_duration_since(marker.telemetry.started_at);
        let latency_ms = u64::try_from(elapsed.num_milliseconds()).map_err(|_| OutcomeCorrelationError::Number)?;
        Ok(Self {
            collected_pending: false,
            resume_claimed: false,
            terminal_id: marker.terminal_id,
            status: marker.status,
            provider_outcome: marker.provider_outcome,
            usage: marker.usage_settlement,
            cost: marker.cost_settlement,
            execution_trace: marker.execution_trace,
            latency_ms,
        })
    }

    /// What a terminal recovered after a restart can be trusted for:
    /// `(status, awaiting_approval, verified)`. The facts are verified only
    /// with a settled execution trace and, for a completed turn, a successful
    /// provider outcome. `directive_kind` is the recorded decision's directive.
    pub(crate) fn recovered_signals(&self, directive_kind: &str) -> (TurnTerminalStatus, bool, bool) {
        let trace = self.execution_trace.as_ref();
        let settled = trace.is_some_and(|trace| {
            !trace.incomplete
                && !trace.provider_attempts_incomplete
                && trace.provider_pending == 0
                && trace.pending_attempts == 0
        });
        let provider_ok = matches!(
            self.provider_outcome.as_ref().map(|outcome| &outcome.status),
            Some(ExecutionStatus::Success | ExecutionStatus::FallbackSuccess)
        );
        let awaiting = directive_kind == "request_approval" || trace.is_some_and(|t| t.awaiting_approval_attempts > 0);
        (
            self.status,
            awaiting,
            settled && (self.status != TurnTerminalStatus::Completed || provider_ok),
        )
    }

    pub(crate) fn from_receipt(
        context: &TurnDecisionContext,
        receipt: &TurnTerminalReceipt,
    ) -> Result<Self, OutcomeCorrelationError> {
        let event = &receipt.terminal_event;
        let trusted = context
            .audit_hops()
            .last()
            .ok_or(OutcomeCorrelationError::Lineage)?
            .state
            .trusted
            .parts();
        if event.id <= 0
            || event.role != "event"
            || event.event_type != "turn.finalized"
            || event.event_id.is_empty()
            || crate::runtime::turn_source::ingress_event_surface(
                &event.source,
                event.channel.as_deref(),
                &event.visibility,
            ) != Some(trusted.surface)
            || event.channel.as_deref() != Some(context.source_channel())
            || event.workspace_id != trusted.workspace_id.as_ref()
            || event.owner_id.as_deref() != trusted.owner_id.as_deref()
            || event.session_key.as_deref() != trusted.session_key.as_deref()
            || event.run_id.as_deref() != trusted.run_id.as_deref()
            || crate::runtime::turn_source::subject_task_id(event.subject.as_ref()) != trusted.task_id.as_deref()
            || event.idempotency_key.as_deref() != Some(format!("turn:{}:final", receipt.terminal_id).as_str())
        {
            return Err(OutcomeCorrelationError::Lineage);
        }
        let marker: TerminalMarker = serde_json::from_str(
            event
                .raw_payload_json
                .as_deref()
                .ok_or(OutcomeCorrelationError::Marker)?,
        )
        .map_err(|_| OutcomeCorrelationError::Marker)?;
        if marker.terminal_id != receipt.terminal_id
            || marker.decision_id.as_deref() != Some(context.decision_id().to_string().as_str())
            || !usage_settlements_agree(marker.usage_settlement.as_ref(), receipt.usage_settlement.as_ref())
            || !cost_settlements_agree(marker.cost_settlement.as_ref(), receipt.cost_settlement.as_ref())
            || marker
                .usage_settlement
                .as_ref()
                .is_some_and(|usage| usage.settlement_id.as_deref() != Some(receipt.terminal_id.as_str()))
        {
            return Err(OutcomeCorrelationError::Marker);
        }
        let elapsed = marker
            .telemetry
            .finished_at
            .signed_duration_since(marker.telemetry.started_at);
        let latency_ms = u64::try_from(elapsed.num_milliseconds()).map_err(|_| OutcomeCorrelationError::Number)?;
        Ok(Self {
            collected_pending: context.collected_pending_event().is_some(),
            resume_claimed: context.claimed_resume_event().is_some(),
            terminal_id: marker.terminal_id,
            status: marker.status,
            provider_outcome: marker.provider_outcome,
            usage: marker.usage_settlement,
            cost: marker.cost_settlement,
            execution_trace: marker.execution_trace,
            latency_ms,
        })
    }
}

/// The usage identity a marker and a receipt must share: settlement id, token
/// counts and priced amount. A replay that carries a different amount or id is
/// a different settlement and is rejected here.
fn usage_settlements_agree(
    marker: Option<&MeteredTokenUsageRecord>,
    receipt: Option<&MeteredTokenUsageRecord>,
) -> bool {
    fn identity(usage: &MeteredTokenUsageRecord) -> (Option<&str>, u64, u64, u64, Option<f64>) {
        (
            usage.settlement_id.as_deref(),
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens,
            usage.cost_usd,
        )
    }
    marker.map(identity) == receipt.map(identity)
}

/// Whether a stored marker's cost settlement and a (re)finalized receipt's
/// describe the same settlement outcome.
///
/// The first finalization stores `Recorded`; replaying the same settlement id
/// over the committed marker returns `Replayed` because the ledger already holds
/// it. Those two are one settlement seen at two times, so they agree. Every
/// other pairing must match exactly in kind, so `UnknownPricing`, `Disabled`
/// and `Failed` never pass for a recorded charge. The amount and id are pinned
/// separately by [`usage_settlements_agree`] and, on replay, by the ledger's own
/// `verify_replayed_settlement`.
fn cost_settlements_agree(
    marker: Option<&crate::cost::types::CostSettlement>,
    receipt: Option<&crate::cost::types::CostSettlement>,
) -> bool {
    use crate::cost::types::CostSettlement::{Recorded, Replayed};
    match (marker, receipt) {
        (Some(Recorded { .. } | Replayed), Some(Recorded { .. } | Replayed)) => true,
        (Some(left), Some(right)) => std::mem::discriminant(left) == std::mem::discriminant(right),
        (None, None) => true,
        _ => false,
    }
}

pub(crate) struct OutcomeCorrelator;

impl OutcomeCorrelator {
    /// The terminal caller invokes this only after `finalize_turn` returns a
    /// durable receipt. Queue admission is not a persistence acknowledgement.
    pub(crate) fn settle(
        writer: &DecisionStoreWriter,
        context: &TurnDecisionContext,
        facts: &TerminalFacts,
    ) -> Result<Enqueue, OutcomeCorrelationError> {
        Self::settle_for(
            writer,
            context.decision_id(),
            context.terminal_outcome_action(),
            context.terminal_outcome_target(),
            facts,
        )
    }

    pub(crate) fn persisted_record(
        decision: &DecisionView,
        facts: &TerminalFacts,
    ) -> Result<OutcomeRecord, OutcomeCorrelationError> {
        let action = decision.selected().and_then(|candidate| {
            super::branch::CausalActionKind::ALL
                .into_iter()
                .find(|kind| kind.as_str() == candidate.action)
        });
        Self::record_for(
            decision.decision_id,
            action,
            decision.selected().map(|candidate| candidate.target.as_str()),
            facts,
        )
    }

    fn settle_for(
        writer: &DecisionStoreWriter,
        decision_id: DecisionId,
        action: Option<super::branch::CausalActionKind>,
        target: Option<&str>,
        facts: &TerminalFacts,
    ) -> Result<Enqueue, OutcomeCorrelationError> {
        Ok(writer.record_outcome(Self::record_for(decision_id, action, target, facts)?))
    }

    fn record_for(
        decision_id: DecisionId,
        action: Option<super::branch::CausalActionKind>,
        target: Option<&str>,
        facts: &TerminalFacts,
    ) -> Result<OutcomeRecord, OutcomeCorrelationError> {
        if facts.status == TurnTerminalStatus::Completed
            && facts
                .execution_trace
                .as_ref()
                .is_some_and(|trace| trace.authorized_silent)
        {
            return Err(OutcomeCorrelationError::Unsupported);
        }
        let classification = if facts.collected_pending && facts.status == TurnTerminalStatus::Completed {
            if action.is_some()
                || target.is_some()
                || facts
                    .execution_trace
                    .as_ref()
                    .is_some_and(|trace| trace.started_attempts != 0 || !trace.calls.is_empty())
                || !facts.provider_outcome.as_ref().is_some_and(|outcome| {
                    matches!(
                        outcome.status,
                        ExecutionStatus::Success | ExecutionStatus::FallbackSuccess
                    )
                })
            {
                return Err(OutcomeCorrelationError::Unsupported);
            }
            OutcomeClassification::AwaitingApproval
        } else if facts.status == TurnTerminalStatus::Silent {
            let confirmed = facts
                .execution_trace
                .as_ref()
                .zip(facts.provider_outcome.as_ref())
                .is_some_and(|(trace, outcome)| trace.confirms_silent(outcome));
            if !confirmed {
                return Err(OutcomeCorrelationError::Unsupported);
            }
            OutcomeClassification::Silent
        } else if matches!(
            action,
            Some(super::branch::CausalActionKind::UseTool | super::branch::CausalActionKind::Delegate)
        ) && facts.status == TurnTerminalStatus::Completed
        {
            let trace = facts
                .execution_trace
                .as_ref()
                .ok_or(OutcomeCorrelationError::Unsupported)?;
            if trace.pending_attempts != 0 || trace.provider_pending != 0 {
                return Err(OutcomeCorrelationError::Unsupported);
            }
            let provider_status = facts.provider_outcome.as_ref().map(|outcome| &outcome.status);
            if trace.awaiting_approval_attempts > 0 {
                OutcomeClassification::AwaitingApproval
            } else if trace.selected_successful_attempts == 0
                || trace.unbound_attempts > 0
                || trace.successful_attempts != trace.started_attempts
                || trace.failed_attempts > 0
                || trace.cancelled_attempts > 0
                || trace.denied_attempts > 0
            {
                OutcomeClassification::Failed
            } else {
                match provider_status {
                    None if action == Some(super::branch::CausalActionKind::UseTool)
                        && facts.resume_claimed
                        && facts.execution_trace.as_ref().is_some_and(|trace| {
                            let capabilities: Vec<_> =
                                trace.calls.iter().map(|call| call.capability_id.clone()).collect();
                            facts.has_completed_resume_evidence(&capabilities)
                        }) =>
                    {
                        OutcomeClassification::Success
                    }
                    Some(ExecutionStatus::Success | ExecutionStatus::FallbackSuccess)
                        if action == Some(super::branch::CausalActionKind::Delegate) =>
                    {
                        OutcomeClassification::Delegated
                    }
                    Some(ExecutionStatus::Success) => OutcomeClassification::Success,
                    Some(ExecutionStatus::FallbackSuccess) => OutcomeClassification::FallbackSuccess,
                    _ => return Err(OutcomeCorrelationError::Unsupported),
                }
            }
        } else {
            classify_terminal(
                action,
                facts.status,
                facts.provider_outcome.as_ref().map(|outcome| &outcome.status),
            )?
        };
        let provider_attempts = facts.provider_outcome.as_ref().map(|outcome| {
            outcome
                .attempts
                .iter()
                .map(|attempt| RecordedOutcomeAttempt {
                    seq: attempt.seq,
                    status: match attempt.status {
                        AttemptStatus::Success => "success",
                        AttemptStatus::Failed => "failed",
                        AttemptStatus::Skipped => "skipped",
                    }
                    .to_owned(),
                    // The source error class is a provider string and may
                    // contain user or credential text. No safe code mapping
                    // exists here, so its observation remains unavailable.
                    error_class: None,
                })
                .collect()
        });
        let cost_micro_usd = facts
            .usage
            .as_ref()
            .and_then(|usage| usage.cost_usd)
            .map(exact_cost_micro)
            .transpose()?;
        let idempotency_status = facts.cost.as_ref().map(|cost| {
            match cost {
                CostSettlement::Recorded { .. } => "recorded",
                CostSettlement::Replayed => "replayed",
                CostSettlement::Disabled => "disabled",
                CostSettlement::UnknownPricing => "unknown_pricing",
                CostSettlement::Failed { .. } => "failed",
            }
            .to_owned()
        });
        let executed = match classification {
            OutcomeClassification::Delegated => Some(("delegate", "session")),
            OutcomeClassification::Success | OutcomeClassification::FallbackSuccess
                if action == Some(super::branch::CausalActionKind::UseTool) =>
            {
                Some(("use_tool", target.ok_or(OutcomeCorrelationError::Unsupported)?))
            }
            OutcomeClassification::Success | OutcomeClassification::FallbackSuccess => Some(("answer", "none")),
            OutcomeClassification::Clarification => Some(("clarify", "none")),
            OutcomeClassification::Refusal => Some(("refuse", "none")),
            OutcomeClassification::Silent
            | OutcomeClassification::Cancelled
            | OutcomeClassification::Failed
            | OutcomeClassification::AwaitingApproval => None,
        };
        let record = OutcomeRecord {
            decision_id,
            settlement_id: facts.terminal_id.clone(),
            recorded_at_ms: chrono::Utc::now().timestamp_millis(),
            classification,
            executed_action: executed.map(|(action, _)| action.to_owned()),
            executed_target: executed.map(|(_, target)| target.to_owned()),
            provider_attempts: facts
                .execution_trace
                .as_ref()
                .and_then(|trace| trace.provider_attempts.clone())
                .or(provider_attempts),
            tool_calls: facts.execution_trace.as_ref().map(|trace| trace.calls.clone()),
            capability_ids: facts.execution_trace.as_ref().map(|trace| {
                trace
                    .calls
                    .iter()
                    .map(|call| call.capability_id.clone())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect()
            }),
            approvals: None,
            tokens_in: facts.usage.as_ref().map(|usage| usage.prompt_tokens),
            tokens_out: facts.usage.as_ref().map(|usage| usage.completion_tokens),
            latency_ms: Some(facts.latency_ms),
            cost_micro_usd,
            idempotency_status,
            reward: None,
            prompt_bytes_before: None,
            prompt_bytes_after: None,
            tool_bytes_before: None,
            tool_bytes_after: None,
            denied_tool_attempts: facts.execution_trace.as_ref().map(|trace| trace.denied_attempts),
            completeness: OutcomeCompleteness::Partial,
            coverage: None,
        };
        Ok(record)
    }
}

const fn classify_terminal(
    action: Option<super::branch::CausalActionKind>,
    status: TurnTerminalStatus,
    provider_status: Option<&ExecutionStatus>,
) -> Result<OutcomeClassification, OutcomeCorrelationError> {
    use super::branch::CausalActionKind;
    match status {
        TurnTerminalStatus::Cancelled => Ok(OutcomeClassification::Cancelled),
        TurnTerminalStatus::Failed => Ok(OutcomeClassification::Failed),
        TurnTerminalStatus::Silent => Err(OutcomeCorrelationError::Unsupported),
        TurnTerminalStatus::Completed => match (action, provider_status) {
            (Some(CausalActionKind::RequestApproval), None) => Ok(OutcomeClassification::AwaitingApproval),
            (Some(CausalActionKind::Clarify), None) => Ok(OutcomeClassification::Clarification),
            (Some(CausalActionKind::Refuse), None) => Ok(OutcomeClassification::Refusal),
            (Some(CausalActionKind::Answer), Some(ExecutionStatus::Success)) => Ok(OutcomeClassification::Success),
            (Some(CausalActionKind::Answer), Some(ExecutionStatus::FallbackSuccess)) => {
                Ok(OutcomeClassification::FallbackSuccess)
            }
            _ => Err(OutcomeCorrelationError::Unsupported),
        },
    }
}

fn exact_cost_micro(usd: f64) -> Result<i64, OutcomeCorrelationError> {
    crate::cost::tracker::ceil_micro_usd(usd)
        .ok()
        .and_then(|micro| i64::try_from(micro).ok())
        .ok_or(OutcomeCorrelationError::Number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal_tree::branch::{CapabilityTarget, CausalActionKind};
    use crate::causal_tree::decision_engine::PredictionAuditTrace;
    use crate::causal_tree::gates::GateOutcome;
    use crate::causal_tree::state::fixtures;
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    fn priced_usage(id: &str, cost: f64) -> MeteredTokenUsageRecord {
        MeteredTokenUsageRecord {
            settlement_id: Some(id.to_owned()),
            provider: "ollama".to_owned(),
            model: "m".to_owned(),
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            source: crate::llm::route_decision::TokenUsageSource::Reported,
            cost_usd: Some(cost),
        }
    }

    fn recorded() -> crate::cost::types::CostSettlement {
        crate::cost::types::CostSettlement::Recorded {
            budget: crate::cost::types::BudgetCheck::Allowed,
        }
    }

    #[test]
    fn replayed_priced_settlement_agrees_with_the_stored_recorded_marker() {
        use crate::cost::types::CostSettlement::Replayed;
        let usage = priced_usage("terminal-1", 0.25);
        assert!(usage_settlements_agree(Some(&usage), Some(&usage.clone())));
        assert!(cost_settlements_agree(Some(&recorded()), Some(&Replayed)));
        assert!(cost_settlements_agree(Some(&Replayed), Some(&recorded())));
        assert!(cost_settlements_agree(Some(&recorded()), Some(&recorded())));
        assert!(cost_settlements_agree(Some(&Replayed), Some(&Replayed)));
        assert!(cost_settlements_agree(None, None));
    }

    #[test]
    fn a_genuinely_different_settlement_is_still_rejected() {
        use crate::cost::types::CostSettlement::{Disabled, Failed, Replayed, UnknownPricing};
        let stored = priced_usage("terminal-1", 0.25);
        let mut other_amount = stored.clone();
        other_amount.cost_usd = Some(0.5);
        let mut other_tokens = stored.clone();
        other_tokens.total_tokens += 1;
        let other_id = priced_usage("terminal-2", 0.25);
        let mut unpriced = stored.clone();
        unpriced.cost_usd = None;
        for different in [&other_amount, &other_tokens, &other_id, &unpriced] {
            assert!(!usage_settlements_agree(Some(&stored), Some(different)));
        }
        assert!(!usage_settlements_agree(Some(&stored), None));
        assert!(!usage_settlements_agree(None, Some(&stored)));
        // Replayed only stands in for Recorded, never for any other outcome.
        for kind in [UnknownPricing, Disabled, Failed { error: "x".to_owned() }] {
            assert!(!cost_settlements_agree(Some(&recorded()), Some(&kind)));
            assert!(!cost_settlements_agree(Some(&Replayed), Some(&kind)));
            assert!(!cost_settlements_agree(Some(&kind), Some(&Replayed)));
        }
        assert!(!cost_settlements_agree(Some(&recorded()), None));
        assert!(!cost_settlements_agree(None, Some(&Replayed)));
    }

    #[test]
    fn collected_pending_classifies_without_executed_effect_and_preserves_failure() {
        let now = chrono::Utc::now();
        let mut facts = TerminalFacts {
            collected_pending: true,
            resume_claimed: false,
            terminal_id: "prx_collection_terminal".into(),
            status: TurnTerminalStatus::Completed,
            provider_outcome: Some(ProviderExecutionOutcome {
                decision_id: "prx_collection_provider".into(),
                started_at: now,
                finished_at: now,
                attempts: Vec::new(),
                final_provider: "prx_fixture".into(),
                final_model: "prx_model".into(),
                status: ExecutionStatus::Success,
                fallback_reason: None,
                tokens_used: Default::default(),
            }),
            usage: None,
            cost: None,
            latency_ms: 0,
            execution_trace: None,
        };
        let record =
            OutcomeCorrelator::record_for(DecisionId::new_runtime(), None, None, &facts).expect("pending evidence");
        assert_eq!(record.classification, OutcomeClassification::AwaitingApproval);
        assert!(record.executed_action.is_none() && record.executed_target.is_none());
        facts.collected_pending = false;
        assert!(OutcomeCorrelator::record_for(DecisionId::new_runtime(), None, None, &facts).is_err());
        facts.collected_pending = true;
        assert!(
            OutcomeCorrelator::record_for(DecisionId::new_runtime(), Some(CausalActionKind::Answer), None, &facts)
                .is_err()
        );
        for (status, expected) in [
            (TurnTerminalStatus::Cancelled, OutcomeClassification::Cancelled),
            (TurnTerminalStatus::Failed, OutcomeClassification::Failed),
        ] {
            facts.status = status;
            let record =
                OutcomeCorrelator::record_for(DecisionId::new_runtime(), None, None, &facts).expect("lifecycle result");
            assert_eq!(record.classification, expected);
            assert!(record.executed_action.is_none());
        }
    }

    #[test]
    fn silent_marker_without_physical_facts_is_unsupported_and_cancellation_still_wins() {
        let mut facts = TerminalFacts {
            collected_pending: false,
            resume_claimed: false,
            terminal_id: "prx_silent_fixture".into(),
            status: TurnTerminalStatus::Silent,
            provider_outcome: None,
            usage: None,
            cost: None,
            latency_ms: 0,
            execution_trace: Some(crate::tools::capability::execution_trace::ExecutionTraceSnapshot {
                authorized_silent: true,
                ..Default::default()
            }),
        };
        assert!(matches!(
            OutcomeCorrelator::record_for(
                DecisionId::new_runtime(),
                Some(CausalActionKind::Answer),
                Some("none"),
                &facts
            ),
            Err(OutcomeCorrelationError::Unsupported)
        ));
        facts.status = TurnTerminalStatus::Completed;
        assert!(matches!(
            OutcomeCorrelator::record_for(
                DecisionId::new_runtime(),
                Some(CausalActionKind::Answer),
                Some("none"),
                &facts
            ),
            Err(OutcomeCorrelationError::Unsupported)
        ));
        for (status, expected) in [
            (TurnTerminalStatus::Cancelled, OutcomeClassification::Cancelled),
            (TurnTerminalStatus::Failed, OutcomeClassification::Failed),
        ] {
            facts.status = status;
            let record = OutcomeCorrelator::record_for(
                DecisionId::new_runtime(),
                Some(CausalActionKind::Answer),
                Some("none"),
                &facts,
            )
            .expect("lifecycle status");
            assert_eq!(record.classification, expected);
            assert!(record.executed_action.is_none());
            assert!(record.executed_target.is_none());
        }
        assert!(
            classify_terminal(
                Some(CausalActionKind::Answer),
                TurnTerminalStatus::Silent,
                Some(&ExecutionStatus::Success)
            )
            .is_err(),
            "raw status cannot mint silent authority"
        );
    }

    #[test]
    fn cancelled_terminal_never_becomes_provider_success() {
        assert_eq!(
            classify_terminal(
                Some(CausalActionKind::Answer),
                TurnTerminalStatus::Cancelled,
                Some(&ExecutionStatus::Success)
            )
            .expect("cancelled classification"),
            OutcomeClassification::Cancelled
        );
        assert_eq!(
            classify_terminal(Some(CausalActionKind::Refuse), TurnTerminalStatus::Completed, None)
                .expect("closed refusal"),
            OutcomeClassification::Refusal
        );
        assert!(
            classify_terminal(Some(CausalActionKind::Answer), TurnTerminalStatus::Completed, None).is_err(),
            "a completed marker without provider evidence cannot prove an answer"
        );
    }

    #[test]
    fn completed_clarify_and_refuse_have_distinct_terminal_outcomes() {
        assert_eq!(
            classify_terminal(Some(CausalActionKind::Clarify), TurnTerminalStatus::Completed, None)
                .expect("selected clarification"),
            OutcomeClassification::Clarification
        );
        assert_eq!(
            classify_terminal(Some(CausalActionKind::Refuse), TurnTerminalStatus::Completed, None)
                .expect("selected refusal"),
            OutcomeClassification::Refusal
        );
        assert!(
            classify_terminal(Some(CausalActionKind::Answer), TurnTerminalStatus::Completed, None).is_err(),
            "an answer still needs provider execution facts"
        );
    }

    #[test]
    fn outcome_cost_uses_the_exact_ledger_ceiling() {
        assert_eq!(exact_cost_micro(3e-6).expect("representable amount"), 4);
        assert_eq!(exact_cost_micro(f64::from_bits(1)).expect("subnormal amount"), 1);
        assert!(exact_cost_micro(f64::MAX).is_err());
        assert!(exact_cost_micro(-1.0).is_err());
    }

    #[test]
    fn built_record_gate_bytes_match_legacy_outer_json_for_all_statuses() {
        let key = fixtures::key();
        for outcome in [
            None,
            Some(GateOutcome::Passed),
            Some(GateOutcome::Blocked {
                gate: 9,
                reason_code: "prx_é",
            }),
            Some(GateOutcome::ApprovalOnly {
                op_ids: vec![crate::causal_tree::branch::OpId::new("prx:exact-write")],
            }),
            Some(GateOutcome::ApprovalDeferred),
        ] {
            let state = fixtures::state(&key, "neutral request");
            let candidate = fixtures::candidate(&key, CausalActionKind::Refuse, CapabilityTarget::None, None).candidate;
            let unselected =
                fixtures::candidate(&key, CausalActionKind::Clarify, CapabilityTarget::None, None).candidate;
            let report = GateReport {
                outcomes: outcome
                    .into_iter()
                    .map(|outcome| (unselected.candidate_id().clone(), outcome))
                    .collect(),
                gate_set_hash: [0; 32],
            };
            // A real abstention requires its selected terminal to pass both
            // admission and commit. Exercise other gate outcomes on the distinct
            // unselected candidate; the None post-rehearsal report stays empty.
            let mut admitted_report = report.clone();
            admitted_report
                .outcomes
                .insert(candidate.candidate_id().clone(), GateOutcome::Passed);
            // Frozen pre-optimization expression: json! serializes each owned
            // gate Value again. The other side below is the real record builder.
            let legacy = json!({
                "pre_prediction": gate_fields(&admitted_report, &key),
                "post_rehearsal": gate_fields(&report, &key),
                "pre_commit": gate_fields(&admitted_report, &key),
            });
            let decision = EngineDecision {
                decision_id: DecisionId::new_runtime(),
                deadline: Instant::now() + Duration::from_secs(1),
                elapsed_ms: 1,
                state,
                candidates: CandidateSet::Legal {
                    candidates: vec![candidate, unselected],
                    exact_mentions: BTreeSet::new(),
                    truncated: Vec::new(),
                    discoverable_families: Default::default(),
                    approval_scopes: BTreeMap::new(),
                    report: admitted_report.clone(),
                },
                selection: Selection::Abstain {
                    action: CausalActionKind::Refuse,
                    reason_code: "hop_stopped",
                },
                exploration_propensities: BTreeMap::new(),
                prediction: None,
                request_compiled: false,
                audit_trace: None,
                fallback_cause: None,
                scores: BTreeMap::new(),
                scored_count: 0,
                rehearsal: BTreeMap::new(),
                post_rehearsal: report.clone(),
                pre_commit: admitted_report,
                call_fingerprints: BTreeMap::new(),
                outgoing_child: None,
                outgoing_parent_allowed: None,
            };
            let record = DecisionRecordBuilder::build_hop(&decision, &key, None, true, false)
                .expect("actual record builder accepts fixture binding");
            assert_eq!(
                serde_json::to_vec(&record.gates).expect("actual gate JSON"),
                serde_json::to_vec(&legacy).expect("legacy gate JSON"),
                "persisted gate bytes must remain exact, including empty/null/string distinctions"
            );
            assert!(record.gates.get("approval_collection").is_none());
        }
    }

    #[test]
    fn forced_choice_stays_out_of_calibration_and_builtin_shadow() {
        let key = fixtures::key();
        let state = fixtures::state(&key, "neutral request");
        let candidate = fixtures::candidate(&key, CausalActionKind::Refuse, CapabilityTarget::None, None).candidate;
        let candidate_id = candidate.candidate_id().clone();
        let gates = GateReport {
            outcomes: BTreeMap::from([(candidate_id, GateOutcome::Passed)]),
            gate_set_hash: [0; 32],
        };
        let trace = PredictionAuditTrace {
            state_fingerprint: state.material_fingerprint(&key),
            policy_version: state.trusted.parts().policy_version.to_string(),
            catalog_generation: state.trusted.parts().catalog_generation.to_hex(),
            answers: BTreeMap::new(),
            omitted_questions: BTreeMap::from([(GlobalQuestion::NextAction.as_str().to_owned(), "forced")]),
            forced_choices: BTreeMap::from([(GlobalQuestion::NextAction.as_str().to_owned(), "refuse".to_owned())]),
            redacted_projection: None,
        };
        let decision = EngineDecision {
            decision_id: DecisionId::new_runtime(),
            deadline: Instant::now() + Duration::from_secs(1),
            elapsed_ms: 1,
            state,
            candidates: CandidateSet::Legal {
                candidates: vec![candidate],
                exact_mentions: BTreeSet::new(),
                truncated: Vec::new(),
                discoverable_families: Default::default(),
                approval_scopes: BTreeMap::new(),
                report: gates.clone(),
            },
            selection: Selection::Abstain {
                action: CausalActionKind::Refuse,
                reason_code: "hop_stopped",
            },
            exploration_propensities: BTreeMap::new(),
            prediction: None,
            request_compiled: false,
            audit_trace: Some(trace),
            fallback_cause: None,
            scores: BTreeMap::new(),
            scored_count: 0,
            rehearsal: BTreeMap::new(),
            post_rehearsal: gates.clone(),
            pre_commit: gates,
            call_fingerprints: BTreeMap::new(),
            outgoing_child: None,
            outgoing_parent_allowed: None,
        };
        let (distributions, features) = audit_fields(&decision).expect("forced code maps only to features");
        assert!(distributions.next_action.is_empty());
        assert!(distributions.capability_target.is_empty());
        assert_eq!(
            features
                .get("forced_choices")
                .and_then(|choices| choices.get(GlobalQuestion::NextAction.as_str())),
            Some(&json!("refuse"))
        );
        assert!(shadow_fields(&decision).next_action.is_none());
        assert!(shadow_fields(&decision).capability_target.is_none());
    }
    #[test]
    fn deterministic_record_keeps_selection_propensity_without_explored_flag() {
        let key = fixtures::key();
        let state = fixtures::state(&key, "neutral request");
        let candidate = fixtures::candidate(&key, CausalActionKind::Answer, CapabilityTarget::None, None).candidate;
        let candidate_id = candidate.candidate_id().clone();
        let gates = GateReport {
            outcomes: BTreeMap::from([(candidate_id.clone(), GateOutcome::Passed)]),
            gate_set_hash: [0; 32],
        };
        let mut decision = EngineDecision {
            decision_id: DecisionId::new_runtime(),
            deadline: Instant::now() + Duration::from_secs(1),
            elapsed_ms: 1,
            state,
            candidates: CandidateSet::Legal {
                candidates: vec![candidate],
                exact_mentions: BTreeSet::new(),
                truncated: Vec::new(),
                discoverable_families: Default::default(),
                approval_scopes: BTreeMap::new(),
                report: gates.clone(),
            },
            selection: Selection::Commit {
                candidate_id: candidate_id.clone(),
                propensity: 1.0,
            },
            exploration_propensities: BTreeMap::new(),
            prediction: None,
            request_compiled: false,
            audit_trace: None,
            fallback_cause: None,
            scores: BTreeMap::new(),
            scored_count: 0,
            rehearsal: BTreeMap::new(),
            post_rehearsal: gates.clone(),
            pre_commit: gates,
            call_fingerprints: BTreeMap::new(),
            outgoing_child: None,
            outgoing_parent_allowed: None,
        };

        let record =
            DecisionRecordBuilder::build_hop(&decision, &key, None, true, false).expect("deterministic record");
        assert_eq!(record.propensity, 1.0);
        assert!(!record.explored);
        assert!(record.candidates.iter().all(|candidate| candidate.propensity.is_none()));
        decision.exploration_propensities.insert(candidate_id.clone(), 0.75);
        decision.selection = Selection::Commit {
            candidate_id,
            propensity: 0.75,
        };
        let sampled =
            DecisionRecordBuilder::build_hop(&decision, &key, None, true, false).expect("recorded sampling metadata");
        assert!(sampled.explored);
        assert_eq!(sampled.propensity, 0.75);
        assert_eq!(
            sampled.candidates.first().expect("one candidate").propensity,
            Some(0.75)
        );
    }
    fn recovered_facts(
        status: TurnTerminalStatus,
        trace: Option<crate::tools::capability::execution_trace::ExecutionTraceSnapshot>,
        provider_success: bool,
    ) -> TerminalFacts {
        let now = chrono::Utc::now();
        TerminalFacts {
            collected_pending: false,
            resume_claimed: false,
            terminal_id: "recovered-terminal".to_owned(),
            status,
            provider_outcome: provider_success.then(|| ProviderExecutionOutcome {
                decision_id: String::new(),
                started_at: now,
                finished_at: now,
                attempts: Vec::new(),
                final_provider: "prx_test".to_owned(),
                final_model: "prx_test".to_owned(),
                status: ExecutionStatus::Success,
                fallback_reason: None,
                tokens_used: Default::default(),
            }),
            usage: None,
            cost: None,
            latency_ms: 0,
            execution_trace: trace,
        }
    }

    #[test]
    fn recovered_terminals_are_verified_only_with_settled_execution_facts() {
        use crate::tools::capability::execution_trace::ExecutionTraceSnapshot;
        let settled = || Some(ExecutionTraceSnapshot::default());
        let signals = |status, trace, provider, directive: &str| {
            recovered_facts(status, trace, provider).recovered_signals(directive)
        };
        // No execution trace: nothing about the run is verifiable.
        for status in [
            TurnTerminalStatus::Completed,
            TurnTerminalStatus::Failed,
            TurnTerminalStatus::Cancelled,
        ] {
            assert_eq!(signals(status, None, true, "answer"), (status, false, false));
        }
        // An unsettled trace is not verifiable either.
        for unsettled in [
            ExecutionTraceSnapshot {
                incomplete: true,
                ..ExecutionTraceSnapshot::default()
            },
            ExecutionTraceSnapshot {
                pending_attempts: 1,
                ..ExecutionTraceSnapshot::default()
            },
            ExecutionTraceSnapshot {
                provider_pending: 1,
                ..ExecutionTraceSnapshot::default()
            },
        ] {
            assert!(!signals(TurnTerminalStatus::Failed, Some(unsettled), false, "answer").2);
        }
        // A completed turn also needs a successful provider outcome.
        assert!(!signals(TurnTerminalStatus::Completed, settled(), false, "answer").2);
        assert_eq!(
            signals(TurnTerminalStatus::Completed, settled(), true, "answer"),
            (TurnTerminalStatus::Completed, false, true)
        );
        // The same approval signals as the live path.
        assert_eq!(
            signals(TurnTerminalStatus::Completed, settled(), true, "request_approval"),
            (TurnTerminalStatus::Completed, true, true)
        );
        assert_eq!(
            signals(TurnTerminalStatus::Failed, settled(), false, "answer"),
            (TurnTerminalStatus::Failed, false, true)
        );
    }
}
