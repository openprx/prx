//! The existing service ledger supplies these observations; they grant nothing.
use super::ExecutionTrace;
use crate::causal_tree::state::exact_calls::{
    BackendCompletion, ExactCallObservation, ExactCallObservations, LedgerDisposition, MAX_EXACT_CALLS,
};
use crate::decision::{DecisionKey, KeyDomain};
use crate::tools::execution::{ToolExecutionCommand, ToolExecutionContext, ToolExecutionStatus};
use std::sync::Arc;

pub(super) fn serialize_observations<S: serde::Serializer>(
    value: &Option<Arc<ExactCallObservations>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&value.as_deref(), serializer)
}

pub(super) fn deserialize_observations<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Arc<ExactCallObservations>>, D::Error> {
    <Option<ExactCallObservations> as serde::Deserialize>::deserialize(deserializer).map(|value| value.map(Arc::new))
}

pub(crate) struct PendingExactCall {
    trace: ExecutionTrace,
    index: usize,
    finished: bool,
}

impl ExecutionTrace {
    pub(crate) fn bind_exact_call_key(&self, key: Arc<DecisionKey>) -> anyhow::Result<()> {
        let mut state = self.0.lock();
        anyhow::ensure!(state.sealed.is_none(), "execution observations are already sealed");
        if let Some(existing) = &state.exact_key {
            anyhow::ensure!(
                Arc::ptr_eq(existing, &key)
                    || existing.hmac(KeyDomain::StateFingerprint, &[b"exact-tool-observation-key.v1"])
                        == key.hmac(KeyDomain::StateFingerprint, &[b"exact-tool-observation-key.v1"]),
                "exact call observation key changed"
            );
        } else {
            state.exact_key = Some(key);
        }
        Ok(())
    }

    pub(crate) fn begin_exact_call(
        &self,
        command: &ToolExecutionCommand,
        context: &ToolExecutionContext,
        input_sha256: &str,
    ) -> Option<PendingExactCall> {
        let supplied_key = command.idempotency_key.as_deref()?;
        let mut state = self.0.lock();
        if state.sealed.is_some() {
            return None;
        }
        let key = state.exact_key.as_ref()?;
        let envelope = &context.envelope;
        let fingerprint = key.hmac(
            KeyDomain::StateFingerprint,
            &[
                b"exact-tool-ledger-observation.v1",
                envelope.workspace_id.as_bytes(),
                &[u8::from(envelope.owner_id.is_some())],
                envelope.owner_id.as_deref().unwrap_or_default().as_bytes(),
                envelope.session_key.as_bytes(),
                context.semantic_turn_id.as_bytes(),
                supplied_key.as_bytes(),
                command.operation_id.as_bytes(),
                command.name.as_bytes(),
                input_sha256.as_bytes(),
                &[u8::from(envelope.config_generation_id.is_some())],
                &envelope.config_generation_id.unwrap_or_default().to_le_bytes(),
                envelope
                    .config_source_revision
                    .as_deref()
                    .unwrap_or_default()
                    .as_bytes(),
            ],
        );
        let facts = state
            .exact_call_observations
            .get_or_insert_with(|| ExactCallObservations {
                version: 1,
                calls: Vec::new(),
                incomplete: false,
            });
        if facts.calls.len() == MAX_EXACT_CALLS {
            facts.incomplete = true;
            return None;
        }
        let index = facts.calls.len();
        facts.calls.push(ExactCallObservation {
            fingerprint,
            ledger: LedgerDisposition::Indeterminate,
            reservation_attempt: None,
            backend_started: false,
            completion: BackendCompletion::Unknown,
        });
        Some(PendingExactCall {
            trace: self.clone(),
            index,
            finished: false,
        })
    }
}

impl PendingExactCall {
    pub(crate) fn reserved(&self, attempt: Option<u32>) {
        self.update(|call| {
            call.ledger = match attempt {
                None => LedgerDisposition::Untracked,
                Some(0) => LedgerDisposition::Reserved,
                Some(_) => LedgerDisposition::UnsentContinuation,
            };
            call.reservation_attempt = attempt;
        });
    }

    pub(crate) fn refused(mut self, status: ToolExecutionStatus, replayed: bool) {
        self.update(|call| {
            call.ledger = if replayed {
                LedgerDisposition::CachedTerminal
            } else if status == ToolExecutionStatus::IdempotencyConflict {
                LedgerDisposition::Conflict
            } else {
                LedgerDisposition::Indeterminate
            };
            call.reservation_attempt = None;
            call.completion = BackendCompletion::NotStarted;
        });
        self.finished = true;
    }

    /// Called only inside the actual backend future, after approval revalidation.
    pub(crate) fn backend_started(&self) {
        self.update(|call| call.backend_started = true);
    }

    pub(crate) fn complete(mut self, status: ToolExecutionStatus) {
        self.update(|call| {
            call.completion = if !call.backend_started {
                BackendCompletion::NotStarted
            } else {
                match status {
                    ToolExecutionStatus::Succeeded => BackendCompletion::Succeeded,
                    ToolExecutionStatus::Failed => BackendCompletion::Failed,
                    ToolExecutionStatus::Cancelled => BackendCompletion::Cancelled,
                    _ => BackendCompletion::Unknown,
                }
            };
        });
        self.finished = true;
    }

    fn update(&self, apply: impl FnOnce(&mut ExactCallObservation)) {
        let mut state = self.trace.0.lock();
        if state.sealed.is_some() {
            return;
        }
        if let Some(call) = state
            .exact_call_observations
            .as_mut()
            .and_then(|facts| facts.calls.get_mut(self.index))
        {
            apply(call);
        }
    }
}

impl Drop for PendingExactCall {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = self.trace.0.lock();
            if state.sealed.is_some() {
                return;
            }
            if let Some(facts) = &mut state.exact_call_observations {
                facts.incomplete = true;
                if let Some(call) = facts.calls.get_mut(self.index) {
                    call.completion = BackendCompletion::Unknown;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::envelope::RuntimeEnvelope;

    #[test]
    fn dropped_started_observation_and_capacity_are_explicitly_incomplete() {
        let trace = ExecutionTrace::default();
        trace.bind_exact_call_key(DecisionKey::ephemeral()).expect("key");
        let context = ToolExecutionContext::new(RuntimeEnvelope::agent("prx_workspace", "prx_session"), "dm");
        let command = ToolExecutionCommand::new("prx_tool", serde_json::json!({})).with_idempotency_key("prx_call");
        let pending = trace
            .begin_exact_call(&command, &context, "digest")
            .expect("observation");
        pending.reserved(Some(0));
        pending.backend_started();
        drop(pending);
        let snapshot = trace.seal();
        let facts = snapshot.exact_call_observations.expect("facts");
        assert!(facts.incomplete);
        assert_eq!(
            facts.calls.first().expect("call").completion,
            BackendCompletion::Unknown
        );
        facts.validate().expect("unknown is explicit");

        let trace = ExecutionTrace::default();
        trace.bind_exact_call_key(DecisionKey::ephemeral()).expect("key");
        for _ in 0..MAX_EXACT_CALLS {
            trace
                .begin_exact_call(&command, &context, "digest")
                .expect("within bound")
                .refused(ToolExecutionStatus::Failed, true);
        }
        assert!(trace.begin_exact_call(&command, &context, "digest").is_none());
        let snapshot = trace.seal();
        let facts = snapshot.exact_call_observations.expect("facts");
        assert_eq!(facts.calls.len(), MAX_EXACT_CALLS);
        assert!(facts.incomplete);
        assert!(
            facts
                .calls
                .iter()
                .all(|call| !call.backend_started && call.completion == BackendCompletion::NotStarted)
        );
    }

    #[test]
    fn missing_facts_omit_the_field_and_strict_reader_rejects_unknown_or_contradictory_facts() {
        let snapshot = ExecutionTrace::default().seal();
        let encoded = serde_json::to_value(&snapshot).expect("legacy trace");
        assert!(encoded.get("exact_call_observations").is_none());
        assert_eq!(
            serde_json::from_value::<super::super::ExecutionTraceSnapshot>(encoded).expect("legacy reader"),
            snapshot
        );
        let call = ExactCallObservation {
            fingerprint: [3; 32],
            ledger: LedgerDisposition::CachedTerminal,
            reservation_attempt: None,
            backend_started: true,
            completion: BackendCompletion::Failed,
        };
        let mut facts = ExactCallObservations {
            version: 1,
            calls: vec![call],
            incomplete: false,
        };
        assert!(
            serde_json::from_value::<ExactCallObservations>(serde_json::to_value(&facts).expect("fixture")).is_err()
        );
        facts.calls.first_mut().expect("call").backend_started = false;
        facts.calls.first_mut().expect("call").completion = BackendCompletion::NotStarted;
        facts.version = 2;
        assert!(
            serde_json::from_value::<ExactCallObservations>(serde_json::to_value(&facts).expect("fixture")).is_err()
        );
        facts.version = 1;
        facts.calls = vec![facts.calls.first().expect("call").clone(); MAX_EXACT_CALLS + 1];
        assert!(
            serde_json::from_value::<ExactCallObservations>(serde_json::to_value(&facts).expect("fixture")).is_err()
        );
    }
}
