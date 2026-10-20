//! Per-run request preparation, retained by the existing issued terminal link.
use super::TurnDecisionContext;
use crate::providers::{catalog::parameters::ResolvedParameters, parameter_capture::Capture};

impl TurnDecisionContext {
    pub(crate) fn model_catalog(&self) -> Option<&crate::providers::catalog::ReleaseCatalog> {
        self.model_catalog.as_deref()
    }

    pub(crate) fn capture_main_parameters(&self, enabled: bool) -> Capture {
        self.main_parameters.get_or_init(|| Capture::new(enabled)).clone()
    }

    pub(crate) fn prepared_main_parameters(&self) -> Vec<ResolvedParameters> {
        self.main_parameters.get().map_or_else(Vec::new, Capture::snapshot)
    }
}
