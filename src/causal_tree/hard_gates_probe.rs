//! The execution admission probe over the legacy hard gates (R11). Part 2
//! removes it together with `gates`.

use super::gates::{GateInputs, HardGates};
use crate::decision_engine::admission_probe::{AdmissionProbe, GenerationError, ProbeOutcome, ProbeReport};
use crate::decision_engine::candidate::CausalCandidate;
use crate::decision_engine::state::CausalState;
use crate::tools::capability::catalog::CatalogView;

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
mod legacy_generator_tests;
