//! Content-free facts from the actual scoped tool ledger and backend poll.
//! Observations never authorize execution or define a generic retry policy.
use serde::{Deserialize, Serialize};

pub(crate) const MAX_EXACT_CALLS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LedgerDisposition {
    /// No ledger lease was issued (including an unavailable read-only ledger).
    Untracked,
    Reserved,
    UnsentContinuation,
    CachedTerminal,
    Conflict,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackendCompletion {
    NotStarted,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExactCallObservation {
    pub(crate) fingerprint: [u8; 32],
    pub(crate) ledger: LedgerDisposition,
    pub(crate) reservation_attempt: Option<u32>,
    pub(crate) backend_started: bool,
    pub(crate) completion: BackendCompletion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireObservations")]
pub(crate) struct ExactCallObservations {
    pub(crate) version: u8,
    pub(crate) calls: Vec<ExactCallObservation>,
    pub(crate) incomplete: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireObservations {
    version: u8,
    #[serde(deserialize_with = "bounded_calls")]
    calls: Vec<ExactCallObservation>,
    incomplete: bool,
}

fn bounded_calls<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<ExactCallObservation>, D::Error> {
    struct Calls;
    impl<'de> serde::de::Visitor<'de> for Calls {
        type Value = Vec<ExactCallObservation>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("at most 64 exact tool observations")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut calls = Vec::new();
            while let Some(call) = sequence.next_element()? {
                if calls.len() == MAX_EXACT_CALLS {
                    return Err(serde::de::Error::custom("exact tool observations exceed bounds"));
                }
                calls.push(call);
            }
            Ok(calls)
        }
    }
    deserializer.deserialize_seq(Calls)
}

impl TryFrom<WireObservations> for ExactCallObservations {
    type Error = &'static str;
    fn try_from(wire: WireObservations) -> Result<Self, Self::Error> {
        let facts = Self {
            version: wire.version,
            calls: wire.calls,
            incomplete: wire.incomplete,
        };
        facts.validate()?;
        Ok(facts)
    }
}

impl ExactCallObservations {
    pub(crate) fn physical_failure_count(&self) -> usize {
        self.calls
            .iter()
            .filter(|call| call.backend_started && call.completion == BackendCompletion::Failed)
            .count()
    }

    pub(crate) fn cached_return_count(&self) -> usize {
        self.calls
            .iter()
            .filter(|call| call.ledger == LedgerDisposition::CachedTerminal)
            .count()
    }

    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.version != 1 || self.calls.len() > MAX_EXACT_CALLS {
            return Err("unsupported or oversized exact tool observations");
        }
        for call in &self.calls {
            let reserved = matches!(
                call.ledger,
                LedgerDisposition::Reserved | LedgerDisposition::UnsentContinuation
            );
            if reserved != call.reservation_attempt.is_some()
                || (call.ledger == LedgerDisposition::Reserved && call.reservation_attempt != Some(0))
                || (call.ledger == LedgerDisposition::UnsentContinuation && call.reservation_attempt == Some(0))
                || (matches!(
                    call.ledger,
                    LedgerDisposition::CachedTerminal | LedgerDisposition::Conflict | LedgerDisposition::Indeterminate
                ) && call.backend_started)
                || (matches!(
                    call.ledger,
                    LedgerDisposition::CachedTerminal | LedgerDisposition::Conflict
                ) && call.completion != BackendCompletion::NotStarted)
                || (call.backend_started && call.completion == BackendCompletion::NotStarted)
                || (!call.backend_started
                    && matches!(
                        call.completion,
                        BackendCompletion::Succeeded | BackendCompletion::Failed | BackendCompletion::Cancelled
                    ))
                || (call.completion == BackendCompletion::Unknown && !self.incomplete)
            {
                return Err("inconsistent exact tool observation");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn absent_observations_keep_legacy_canonical_and_present_facts_bind_both_fingerprints() {
        let key = super::super::fixtures::key();
        let mut state = super::super::fixtures::state(&key, "prx observation");
        let original = [state.canonical(false), state.canonical(true)];
        state.observed = state.observed.clone().with_exact_call_observations(None);
        assert_eq!([state.canonical(false), state.canonical(true)], original);
        for value in &original {
            assert!(!value.contains("exact_call_observations"));
        }
        let facts = ExactCallObservations {
            version: 1,
            incomplete: false,
            calls: vec![ExactCallObservation {
                fingerprint: [9; 32],
                ledger: LedgerDisposition::Reserved,
                reservation_attempt: Some(0),
                backend_started: true,
                completion: BackendCompletion::Failed,
            }],
        };
        state.observed = state
            .observed
            .clone()
            .with_exact_call_observations(Some(Arc::new(facts.clone())));
        let before = [
            state.material_fingerprint(&key),
            state.record_fingerprint(&key),
            state.evidence_fingerprint(&key),
        ];
        let mut changed = facts;
        changed.calls.first_mut().expect("actual fact").fingerprint = [8; 32];
        state.observed = state
            .observed
            .clone()
            .with_exact_call_observations(Some(Arc::new(changed)));
        assert_ne!(
            state.material_fingerprint(&key),
            before.first().expect("material").as_str()
        );
        assert_ne!(state.record_fingerprint(&key), before.get(1).expect("record").as_str());
        assert_ne!(
            state.evidence_fingerprint(&key),
            before.get(2).expect("evidence").as_str()
        );
    }
}
