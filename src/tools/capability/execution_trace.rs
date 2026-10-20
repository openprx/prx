//! Bounded content-free observations from the actual execution service.
use crate::causal_tree::store::{MAX_RECORDED_PROVIDER_ATTEMPTS, RecordedOutcomeAttempt, RecordedToolCall};
use crate::llm::route_decision::{AttemptStatus, ProviderAttempt};
use crate::tools::execution::ToolExecutionStatus;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
mod exact_calls;
use crate::causal_tree::state::exact_calls::ExactCallObservations;

const MAX_CALLS: usize = 64;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionTraceSnapshot {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "exact_calls::serialize_observations",
        deserialize_with = "exact_calls::deserialize_observations"
    )]
    pub(crate) exact_call_observations: Option<Arc<ExactCallObservations>>,
    #[serde(default)]
    pub(crate) provider_attempts: Option<Vec<RecordedOutcomeAttempt>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) authorized_silent: bool,
    #[serde(default)]
    pub(crate) provider_requests: u64,
    #[serde(default)]
    pub(crate) provider_pending: u64,
    #[serde(default)]
    pub(crate) provider_attempts_incomplete: bool,
    pub(crate) calls: Vec<RecordedToolCall>,
    pub(crate) started_attempts: u64,
    pub(crate) successful_attempts: u64,
    #[serde(default)]
    pub(crate) selected_successful_attempts: u64,
    pub(crate) failed_attempts: u64,
    pub(crate) cancelled_attempts: u64,
    pub(crate) pending_attempts: u64,
    pub(crate) denied_attempts: u64,
    #[serde(default)]
    pub(crate) awaiting_approval_attempts: u64,
    pub(crate) unbound_attempts: u64,
    pub(crate) incomplete: bool,
}

#[derive(Debug, Default)]
struct State {
    exact_key: Option<Arc<crate::decision::DecisionKey>>,
    exact_call_observations: Option<ExactCallObservations>,
    authorized_silent: bool,
    provider_attempts: Vec<RecordedOutcomeAttempt>,
    provider_requests: u64,
    provider_pending: u64,
    provider_attempts_incomplete: bool,
    calls: BTreeMap<String, RecordedToolCall>,
    started_attempts: u64,
    successful_attempts: u64,
    selected_successful_attempts: u64,
    failed_attempts: u64,
    cancelled_attempts: u64,
    pending_attempts: u64,
    denied_attempts: u64,
    awaiting_approval_attempts: u64,
    unbound_attempts: u64,
    incomplete: bool,
    sealed: Option<ExecutionTraceSnapshot>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExecutionTrace(Arc<Mutex<State>>);
impl PartialEq for ExecutionTrace {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ExecutionTrace {}

pub(crate) struct PendingExecution {
    trace: ExecutionTrace,
    operation_id: Option<String>,
    active: bool,
    selected: bool,
}

pub(crate) struct PendingProvider {
    trace: ExecutionTrace,
    active: bool,
}

impl ExecutionTrace {
    /// Observe the actual provider invocation without persisting labels or
    /// payloads. A cancelled future leaves an explicit incomplete observation.
    pub(crate) fn begin_provider(&self) -> anyhow::Result<PendingProvider> {
        let mut state = self.0.lock();
        anyhow::ensure!(state.sealed.is_none(), "decision execution is already closed");
        state.provider_requests = state.provider_requests.saturating_add(1);
        state.provider_pending = state.provider_pending.saturating_add(1);
        Ok(PendingProvider {
            trace: self.clone(),
            active: true,
        })
    }
    /// Only the source-bound runtime dependency gate records intentional silence.
    /// This is not a tool dispatch or successful execution receipt.
    pub(crate) fn record_authorized_silence(&self) -> anyhow::Result<()> {
        let mut state = self.0.lock();
        anyhow::ensure!(
            state.sealed.is_none()
                && !state.authorized_silent
                && state.provider_requests > 0
                && state.provider_pending == 0
                && !state.provider_attempts_incomplete
                && state.pending_attempts == 0
                && !state.incomplete
                && state
                    .provider_attempts
                    .iter()
                    .rev()
                    .find(|attempt| attempt.status != "skipped")
                    .is_some_and(|attempt| attempt.status == "success"),
            "intentional silence requires a completed actual provider invocation"
        );
        state.authorized_silent = true;
        Ok(())
    }

    pub(crate) fn validate_terminal_status(
        &self,
        status: crate::agent::terminal::TurnTerminalStatus,
        outcome: Option<&crate::llm::route_decision::ProviderExecutionOutcome>,
    ) -> anyhow::Result<()> {
        use crate::agent::terminal::TurnTerminalStatus;
        let state = self.0.lock();
        anyhow::ensure!(
            !state.authorized_silent || status != TurnTerminalStatus::Completed,
            "authorized silence cannot be relabeled as a completed answer"
        );
        if status == TurnTerminalStatus::Silent {
            anyhow::ensure!(
                state
                    .sealed
                    .as_ref()
                    .zip(outcome)
                    .is_some_and(|(trace, outcome)| trace.confirms_silent(outcome)),
                "silent terminal lacks confirmed runtime execution facts"
            );
        }
        Ok(())
    }

    pub(crate) fn is_sealed(&self) -> bool {
        self.0.lock().sealed.is_some()
    }

    pub(crate) fn begin(&self, operation_id: &str, capability_id: Option<&str>, selected: bool) -> PendingExecution {
        let mut state = self.0.lock();
        let active = state.sealed.is_none();
        if active {
            state.started_attempts = state.started_attempts.saturating_add(1);
            state.pending_attempts = state.pending_attempts.saturating_add(1);
        }
        let operation_id = if !active {
            None
        } else if let Some(id) = capability_id {
            if state.calls.len() >= MAX_CALLS || state.calls.contains_key(operation_id) {
                state.incomplete = true;
                None
            } else {
                state.calls.insert(
                    operation_id.to_owned(),
                    RecordedToolCall {
                        capability_id: id.to_owned(),
                        status: "pending".to_owned(),
                    },
                );
                Some(operation_id.to_owned())
            }
        } else {
            state.unbound_attempts = state.unbound_attempts.saturating_add(1);
            None
        };
        PendingExecution {
            trace: self.clone(),
            operation_id,
            active,
            selected: selected && capability_id.is_some(),
        }
    }

    /// The first terminal snapshot is immutable across retries and cancellation.
    pub(crate) fn seal(&self) -> ExecutionTraceSnapshot {
        let mut state = self.0.lock();
        if let Some(snapshot) = &state.sealed {
            return snapshot.clone();
        }
        let mut incomplete = state.incomplete;
        let calls = state
            .calls
            .values()
            .cloned()
            .map(|mut call| {
                if call.status == "pending" {
                    incomplete = true;
                    call.status = "cancelled".to_owned();
                }
                call
            })
            .collect();
        let snapshot = ExecutionTraceSnapshot {
            exact_call_observations: state.exact_call_observations.clone().map(|mut facts| {
                facts.incomplete |= facts
                    .calls
                    .iter()
                    .any(|call| call.completion == crate::causal_tree::state::exact_calls::BackendCompletion::Unknown);
                Arc::new(facts)
            }),
            authorized_silent: state.authorized_silent,
            provider_attempts: Some(state.provider_attempts.clone()),
            provider_requests: state.provider_requests,
            provider_pending: state.provider_pending,
            provider_attempts_incomplete: state.provider_attempts_incomplete || state.provider_pending != 0,
            calls,
            started_attempts: state.started_attempts,
            successful_attempts: state.successful_attempts,
            selected_successful_attempts: state.selected_successful_attempts,
            failed_attempts: state.failed_attempts,
            cancelled_attempts: state.cancelled_attempts,
            pending_attempts: state.pending_attempts,
            denied_attempts: state.denied_attempts,
            awaiting_approval_attempts: state.awaiting_approval_attempts,
            unbound_attempts: state.unbound_attempts,
            incomplete,
        };
        state.sealed = Some(snapshot.clone());
        snapshot
    }
}

impl PendingProvider {
    pub(crate) fn finish(mut self, attempts: Option<&[ProviderAttempt]>) {
        self.record(attempts);
        self.active = false;
    }

    /// A runtime cancellation proves this request did not return a usable
    /// response, even though provider-side usage can remain unknown. Record a
    /// bounded failed observation so a serial successor can inherit the
    /// completed control-flow fact without claiming a successful response.
    pub(crate) fn cancelled(mut self) {
        self.record_failed();
        self.active = false;
    }

    /// The provider invocation returned an error without a complete attempt
    /// trace. Record the completed failure without leaving terminal evidence
    /// pending; no successful response can be claimed from this observation.
    pub(crate) fn failed(mut self) {
        self.record_failed();
        self.active = false;
    }

    fn record_failed(&self) {
        let mut state = self.trace.0.lock();
        if state.sealed.is_none() {
            state.provider_pending = state.provider_pending.saturating_sub(1);
            if state.provider_attempts.len() >= MAX_RECORDED_PROVIDER_ATTEMPTS {
                state.provider_attempts_incomplete = true;
            } else {
                let seq = state.provider_attempts.len() as u32 + 1;
                state.provider_attempts.push(RecordedOutcomeAttempt {
                    seq,
                    status: "failed".to_owned(),
                    error_class: None,
                });
            }
        }
    }

    fn record(&self, attempts: Option<&[ProviderAttempt]>) {
        let mut state = self.trace.0.lock();
        if state.sealed.is_some() {
            return;
        }
        state.provider_pending = state.provider_pending.saturating_sub(1);
        let Some(attempts) = attempts.filter(|attempts| !attempts.is_empty()) else {
            // Provider errors do not currently carry a complete attempt trace.
            // Retain earlier observations without inventing a failed attempt.
            state.provider_attempts_incomplete = true;
            return;
        };
        for attempt in attempts {
            if state.provider_attempts.len() >= MAX_RECORDED_PROVIDER_ATTEMPTS {
                state.provider_attempts_incomplete = true;
                break;
            }
            let seq = state.provider_attempts.len() as u32 + 1;
            state.provider_attempts.push(RecordedOutcomeAttempt {
                seq,
                status: match attempt.status {
                    AttemptStatus::Success => "success",
                    AttemptStatus::Failed => "failed",
                    AttemptStatus::Skipped => "skipped",
                }
                .to_owned(),
                error_class: None,
            });
        }
    }
}

impl Drop for PendingProvider {
    fn drop(&mut self) {
        if self.active {
            self.record(None);
        }
    }
}

impl PendingExecution {
    pub(crate) fn finish(mut self, status: ToolExecutionStatus) {
        self.record(status);
        self.active = false;
    }
    fn record(&self, status: ToolExecutionStatus) {
        let mut state = self.trace.0.lock();
        if state.sealed.is_some() {
            return;
        }
        state.pending_attempts = state.pending_attempts.saturating_sub(1);
        let code = match status {
            ToolExecutionStatus::Succeeded => {
                state.successful_attempts = state.successful_attempts.saturating_add(1);
                if self.selected {
                    state.selected_successful_attempts = state.selected_successful_attempts.saturating_add(1);
                }
                "success"
            }
            ToolExecutionStatus::Cancelled => {
                state.cancelled_attempts = state.cancelled_attempts.saturating_add(1);
                "cancelled"
            }
            ToolExecutionStatus::AwaitingApproval => {
                state.awaiting_approval_attempts = state.awaiting_approval_attempts.saturating_add(1);
                state.incomplete = true;
                "awaiting_approval"
            }
            ToolExecutionStatus::Denied
            | ToolExecutionStatus::ApprovalDenied
            | ToolExecutionStatus::PreparationDenied
            | ToolExecutionStatus::StaleCapability => {
                state.denied_attempts = state.denied_attempts.saturating_add(1);
                "denied"
            }
            _ => {
                state.failed_attempts = state.failed_attempts.saturating_add(1);
                "failed"
            }
        };
        if let Some(call) = self.operation_id.as_ref().and_then(|id| state.calls.get_mut(id)) {
            call.status = code.to_owned();
        }
    }
}
impl Drop for PendingExecution {
    fn drop(&mut self) {
        if self.active {
            self.record(ToolExecutionStatus::Cancelled);
        }
    }
}

impl ExecutionTraceSnapshot {
    /// Signed terminal replay must retain both the runtime observation and its
    /// completed physical provider facts. A raw status or boolean is insufficient.
    pub(crate) fn confirms_silent(&self, outcome: &crate::llm::route_decision::ProviderExecutionOutcome) -> bool {
        use crate::llm::route_decision::ExecutionStatus;
        let Some(recorded) = self.provider_attempts.as_ref() else {
            return false;
        };
        if !self.authorized_silent
            || self.incomplete
            || self.provider_requests == 0
            || self.provider_pending != 0
            || self.provider_attempts_incomplete
            || self.pending_attempts != 0
            || self.unbound_attempts != 0
            || recorded.is_empty()
            || recorded.len() > MAX_RECORDED_PROVIDER_ATTEMPTS
            || self.provider_requests > recorded.len() as u64
            || outcome.attempts.is_empty()
            || outcome.attempts.len() > recorded.len()
            || !matches!(
                outcome.status,
                ExecutionStatus::Success | ExecutionStatus::FallbackSuccess
            )
        {
            return false;
        }
        if recorded.iter().enumerate().any(|(index, attempt)| {
            attempt.seq != index as u32 + 1 || !matches!(attempt.status.as_str(), "success" | "failed" | "skipped")
        }) {
            return false;
        }
        let Some(suffix) = recorded.get(recorded.len() - outcome.attempts.len()..) else {
            return false;
        };
        if suffix
            .iter()
            .zip(&outcome.attempts)
            .enumerate()
            .any(|(index, (observed, actual))| {
                actual.seq != index as u32 + 1
                    || observed.status
                        != match actual.status {
                            AttemptStatus::Success => "success",
                            AttemptStatus::Failed => "failed",
                            AttemptStatus::Skipped => "skipped",
                        }
            })
        {
            return false;
        }
        outcome
            .attempts
            .iter()
            .rev()
            .find(|attempt| attempt.status != AttemptStatus::Skipped)
            .is_some_and(|attempt| {
                attempt.status == AttemptStatus::Success
                    && attempt.provider == outcome.final_provider
                    && attempt.model == outcome.final_model
                    && attempt.finished_at >= attempt.started_at
                    && outcome.finished_at >= outcome.started_at
            })
    }
}

#[cfg(test)]
mod silent_tests {
    use super::*;
    use crate::llm::route_decision::{ExecutionStatus, ProviderExecutionOutcome};

    fn outcome() -> ProviderExecutionOutcome {
        let now = chrono::Utc::now();
        ProviderExecutionOutcome {
            prepared_parameters: Vec::new(),
            decision_id: "prx_silent_fixture".into(),
            started_at: now,
            finished_at: now,
            attempts: [AttemptStatus::Failed, AttemptStatus::Skipped, AttemptStatus::Success]
                .into_iter()
                .enumerate()
                .map(|(index, status)| ProviderAttempt {
                    seq: index as u32 + 1,
                    provider: "prx_provider".into(),
                    model: "prx_model".into(),
                    started_at: now,
                    finished_at: now,
                    status,
                    error_class: None,
                    error_message: None,
                })
                .collect(),
            final_provider: "prx_provider".into(),
            final_model: "prx_model".into(),
            status: ExecutionStatus::FallbackSuccess,
            fallback_reason: None,
            tokens_used: Default::default(),
        }
    }

    #[test]
    fn silent_observation_requires_finished_actual_provider_and_is_not_tool_success() {
        let trace = ExecutionTrace::default();
        assert!(trace.record_authorized_silence().is_err());
        let pending = trace.begin_provider().expect("begin actual invocation");
        assert!(trace.record_authorized_silence().is_err());
        let outcome = outcome();
        pending.finish(Some(&outcome.attempts));
        trace
            .record_authorized_silence()
            .expect("finished fallback can be silent");
        let snapshot = trace.seal();
        assert!(snapshot.confirms_silent(&outcome));
        assert!(
            trace
                .validate_terminal_status(crate::agent::terminal::TurnTerminalStatus::Completed, Some(&outcome))
                .is_err()
        );
        assert!(
            trace
                .validate_terminal_status(crate::agent::terminal::TurnTerminalStatus::Silent, Some(&outcome))
                .is_ok()
        );
        for status in [
            crate::agent::terminal::TurnTerminalStatus::Failed,
            crate::agent::terminal::TurnTerminalStatus::Cancelled,
        ] {
            assert!(trace.validate_terminal_status(status, Some(&outcome)).is_ok());
        }
        assert_eq!(snapshot.started_attempts, 0);
        assert_eq!(snapshot.successful_attempts, 0);
        assert!(snapshot.calls.is_empty(), "expected an empty value");
        assert!(trace.record_authorized_silence().is_err());
        let mut changed = outcome.clone();
        changed.final_model = "prx_other_model".into();
        assert!(!snapshot.confirms_silent(&changed));
        changed = outcome.clone();
        changed.attempts.clear();
        assert!(!snapshot.confirms_silent(&changed));
        let mut legacy = snapshot.clone();
        legacy.authorized_silent = false;
        assert!(!legacy.confirms_silent(&outcome));
        let mut forged = ExecutionTraceSnapshot {
            authorized_silent: true,
            ..Default::default()
        };
        assert!(!forged.confirms_silent(&outcome));
        forged = snapshot;
        forged.provider_pending = 1;
        assert!(!forged.confirms_silent(&outcome));
    }

    #[test]
    fn provider_retention_preserves_counts_and_marks_overflow_without_authorizing_silence() {
        let final_outcome = outcome();
        let attempt = final_outcome
            .attempts
            .last()
            .expect("actual successful fixture attempt");
        for count in [128_u64, 129, 1001] {
            let trace = ExecutionTrace::default();
            for _ in 0..count {
                trace
                    .begin_provider()
                    .expect("provider admission")
                    .finish(Some(std::slice::from_ref(attempt)));
            }
            if count > 128 {
                assert!(trace.record_authorized_silence().is_err());
            }
            let snapshot = trace.seal();
            assert_eq!(snapshot.provider_requests, count);
            assert_eq!(snapshot.provider_pending, 0);
            assert_eq!(snapshot.provider_attempts_incomplete, count > 128);
            let retained = snapshot.provider_attempts.as_ref().expect("actual observations");
            assert_eq!(retained.len(), 128);
            assert!(
                retained
                    .iter()
                    .enumerate()
                    .all(|(index, item)| item.seq == index as u32 + 1 && item.status == "success")
            );
            assert!(!snapshot.authorized_silent);
            if count > 128 {
                assert!(!snapshot.confirms_silent(&final_outcome));
            }
        }
    }

    #[test]
    fn ordinary_trace_omits_silent_marker_and_legacy_missing_marker_defaults_false() {
        let snapshot = ExecutionTrace::default().seal();
        let encoded = serde_json::to_value(&snapshot).expect("encode");
        assert!(encoded.get("authorized_silent").is_none());
        let decoded: ExecutionTraceSnapshot = serde_json::from_value(encoded).expect("legacy shape");
        assert!(!decoded.authorized_silent);
        assert_eq!(snapshot, decoded);
    }

    #[test]
    fn dropped_provider_cannot_issue_silent_observation() {
        let trace = ExecutionTrace::default();
        drop(trace.begin_provider().expect("begin"));
        assert!(trace.record_authorized_silence().is_err());
        assert!(trace.seal().provider_attempts_incomplete);
    }
}
