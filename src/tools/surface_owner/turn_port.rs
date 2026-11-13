//! The port through which execution-owned surface code reads one live turn.
//!
//! The turn runtime implements it for an opened turn; the surface owner,
//! discovery and execution-binding code read only these facts, never an
//! engine decision or its directive.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::providers::catalog::ResolvedModel;
use crate::runtime::turn_source::TurnSourceProof;
use crate::tools::capability::admission_facts::AdmissionFactsV1;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogView, SurfacePolicy};
use crate::tools::capability::contract::{CapabilityRef, CatalogGeneration};
use crate::tools::capability::resume_evidence::VerifiedResumeEvidenceV1;
use crate::tools::capability::search_tool::DiscoveryState;
use crate::tools::capability::turn_state::TrustedParts;
use crate::tools::execution::ToolExecutionService;

use super::publication::PublicationRules;

pub(crate) trait TurnSurfacePort {
    /// Recheck that `proof` still binds this opened turn (publication,
    /// actor, model, live parity, provenance, input and catalog generation)
    /// and return the serving model.
    fn validate_turn(&self, proof: &dyn TurnSourceProof) -> anyhow::Result<ResolvedModel>;
    /// The serving model resolved for `proof`, without the turn recheck.
    fn resolve_model(&self, proof: &dyn TurnSourceProof) -> anyhow::Result<ResolvedModel>;
    fn model_can_call_tools(&self, proof: &dyn TurnSourceProof) -> bool;
    /// Re-authenticate the historical resume evidence of this turn, if any.
    fn fresh_resume_evidence(&self, proof: &dyn TurnSourceProof) -> anyhow::Result<Option<VerifiedResumeEvidenceV1>>;
    fn catalog(&self) -> &Arc<CapabilityCatalog>;
    fn publication_policy(&self) -> &SurfacePolicy;
    fn execution(&self) -> &Arc<ToolExecutionService>;
    fn config(&self) -> &crate::config::Config;
    /// The trusted scope pinned when the turn opened.
    fn trusted(&self) -> &TrustedParts;
    fn facts(&self) -> &AdmissionFactsV1;
    fn routing_input(&self) -> &str;
    /// The surface published when the turn opened.
    fn execution_refs(&self) -> &BTreeSet<CapabilityRef>;
    /// Whether the turn sealed an outgoing child manifest.
    fn outgoing_child(&self) -> bool;
    fn execution_closed(&self) -> &Arc<AtomicBool>;
    fn discovery(&self) -> &DiscoveryState;

    fn catalog_generation(&self) -> CatalogGeneration {
        self.trusted().catalog_generation
    }

    fn catalog_view(&self) -> CatalogView<'_> {
        self.catalog()
            .view(&CatalogScope::default())
            .with_surface(self.publication_policy())
    }
}

/// The publication rules of the port's turn over `view`.
pub(crate) fn rules<'a, 'c>(port: &'a dyn TurnSurfacePort, view: &'a CatalogView<'c>) -> PublicationRules<'a, 'c> {
    PublicationRules {
        trusted: port.trusted(),
        facts: port.facts(),
        catalog: view,
    }
}
