//! The prediction half of the retired decision hop (R21): the borrowed
//! prediction port the advisory driver predicts through, and the derived
//! state the question compiler checks a candidate set against.

use std::sync::Arc;

use crate::cost::decision_attempt::DecisionError;
use crate::decision_engine::candidate_generator::CandidateSet;
use crate::decision_engine::prediction::factory::PredictionResult;
use crate::decision_engine::state::{DerivedCandidate, DerivedState, DuplicateCandidate};
use crate::observability::causal_event::CausalLineage;
use crate::tools::capability::catalog::CatalogView;
use crate::tools::capability::contract::CausalActionKind;

/// Borrowed prediction port owned by the engine domain. Its producer keeps
/// physical accounting and transport outside the engine; no boxed future is
/// introduced on builtin/cache or physical paths by this contract.
pub(crate) mod prediction_sealed {
    pub(crate) trait Producer {}
}

pub(crate) trait PredictionPort: prediction_sealed::Producer + Clone + Send + Sync {
    fn predict<'a>(
        &'a self,
        request: &'a crate::decision_engine::prediction::DecisionRequest,
        lineage: &'a CausalLineage,
    ) -> impl std::future::Future<Output = Result<PredictionResult, DecisionError>> + Send + 'a;
}

/// The derived state of an admitted candidate set: what the question
/// compiler projects and checks against the set before sealing a request.
pub(crate) fn derive_state(set: &CandidateSet, catalog: &CatalogView<'_>) -> Result<DerivedState, DuplicateCandidate> {
    let CandidateSet::Legal {
        candidates,
        exact_mentions,
    } = set
    else {
        return DerivedState::new(Vec::new());
    };
    DerivedState::new(
        candidates
            .iter()
            .map(|candidate| DerivedCandidate {
                candidate: candidate.clone(),
                summary: if candidate.action() == CausalActionKind::UseTool
                    && candidate.target() == crate::tools::capability::contract::CapabilityTarget::Filesystem
                    && candidate.required_capabilities().len() > 1
                {
                    let names = candidate
                        .required_capabilities()
                        .iter()
                        .filter_map(|reference| catalog.entry_any(&reference.id))
                        .map(|entry| entry.public_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Arc::from(format!("Native workspace file operations: {names}"))
                } else {
                    Arc::from(
                        candidate
                            .required_capabilities()
                            .first()
                            .and_then(|reference| catalog.entry_any(&reference.id))
                            .map_or("Fixed terminal", |entry| entry.summary.as_str()),
                    )
                },
                exact_mention: exact_mentions.contains(candidate.candidate_id()),
                available: true,
            })
            .collect(),
    )
}
