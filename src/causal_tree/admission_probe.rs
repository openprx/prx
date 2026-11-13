//! Admission seam between candidate generation and the execution hard gates.
//!
//! The generator asks an [`AdmissionProbe`] whether a structural candidate is
//! admitted; it never reads gate inputs itself. [`HardGatesProbe`] is the
//! execution-owned implementation over the legacy hard gates (Part 2 removes
//! it together with `gates`). [`AdvisoryProbe`] wraps an execution probe for
//! the advisory engine: it reports the same outcomes but `filters()` is false,
//! so admission only annotates the analysed candidates and never removes one.

use std::collections::BTreeMap;

use super::branch::{CandidateId, CausalCandidate};
use super::gates::{GateInputs, HardGates};
use super::state::CausalState;
use crate::tools::capability::catalog::CatalogView;
use crate::tools::capability::contract::OpId;

/// The admission of one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Admission {
    Passed,
    Blocked { gate: u8, reason_code: &'static str },
    ApprovalOnly { op_ids: Vec<OpId> },
    ApprovalDeferred,
}

/// One candidate's probe result: its admission and whether the runtime bound
/// an exact call scope (a verified call fingerprint) to it. The two facts are
/// independent: an approval outcome never implies an exact call scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProbeOutcome {
    pub admission: Admission,
    pub exact_call_scope: bool,
}

/// The admissions of a whole candidate set.
#[derive(Debug, Clone)]
pub(crate) struct ProbeReport {
    pub outcomes: BTreeMap<CandidateId, Admission>,
}

/// Why a candidate set could not be generated. The legacy hard gates use the
/// same type (`gates::GateError`), so every gate error maps 1:1.
#[derive(Debug, thiserror::Error)]
pub(crate) enum GenerationError {
    #[error("duplicate candidate id in gate input")]
    DuplicateCandidate,
    #[error("candidate set has no refusal path")]
    MissingRefusal,
    #[error("candidate limit must reserve at least one working and two terminal paths")]
    InvalidCandidateLimit,
}

/// Admission of structural candidates, asked by the generator.
pub(crate) trait AdmissionProbe {
    fn check_one(&self, candidate: &CausalCandidate) -> ProbeOutcome;
    fn check_all(&self, candidates: &[CausalCandidate]) -> Result<ProbeReport, GenerationError>;
    /// Whether an admission outcome may exclude, close, substitute or
    /// de-prioritise a candidate. `false` keeps every outcome as annotation.
    fn filters(&self) -> bool;
}

/// The advisory admission: the outcomes of the wrapped execution probe, used
/// only as annotations (`filters() == false`). The advice therefore describes
/// the real hard-gate outcome of every structural candidate.
pub(crate) struct AdvisoryProbe<'a> {
    pub inner: &'a dyn AdmissionProbe,
}

impl AdmissionProbe for AdvisoryProbe<'_> {
    fn check_one(&self, candidate: &CausalCandidate) -> ProbeOutcome {
        self.inner.check_one(candidate)
    }

    fn check_all(&self, candidates: &[CausalCandidate]) -> Result<ProbeReport, GenerationError> {
        self.inner.check_all(candidates)
    }

    fn filters(&self) -> bool {
        false
    }
}

/// The execution admission: the hard gates over one pinned state, catalog
/// and set of execution-owned gate inputs.
pub(crate) struct HardGatesProbe<'a, 'catalog, 'gates> {
    pub state: &'a CausalState,
    pub catalog: &'a CatalogView<'catalog>,
    pub gates: &'a GateInputs<'gates>,
}

impl AdmissionProbe for HardGatesProbe<'_, '_, '_> {
    fn check_one(&self, candidate: &CausalCandidate) -> ProbeOutcome {
        ProbeOutcome {
            admission: HardGates::check_one(self.state, self.catalog, candidate, self.gates),
            exact_call_scope: self.gates.call_fingerprints.contains_key(candidate.candidate_id()),
        }
    }

    fn check_all(&self, candidates: &[CausalCandidate]) -> Result<ProbeReport, GenerationError> {
        Ok(HardGates::apply(self.state, self.catalog, candidates, self.gates)?)
    }

    fn filters(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests;
