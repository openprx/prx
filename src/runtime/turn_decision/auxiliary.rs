//! Concrete auxiliary target selection after actual source/context admission.
use super::{TurnDecisionContext, TurnDecisionService, TurnDisposition, TurnSourceProof};
use crate::providers::catalog::ResolvedModel;
use crate::providers::catalog::{Requirement, UnknownPolicy};
use crate::runtime::auxiliary_call::{AuxiliaryAccounting, AuxiliaryPurpose};
use std::sync::Arc;

impl TurnDecisionService {
    pub(crate) fn auxiliary_accounting(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        purpose: AuxiliaryPurpose,
    ) -> anyhow::Result<Arc<AuxiliaryAccounting>> {
        anyhow::ensure!(
            purpose != AuxiliaryPurpose::SmartGroupClassifier,
            "smart-group classifier requires its configured selection"
        );
        let (requested, selected) = self.resolve_auxiliary_target(proof, context, purpose, None)?;
        let provider = crate::providers::create_provider_for(
            &selected,
            &self.provider_catalog,
            &self.generation.effective.reliability,
            &self.generation.effective.cost,
            &crate::providers::provider_runtime_options_from_config(&self.generation.effective),
        )
        .map_err(|_| anyhow::anyhow!("auxiliary provider construction refused"))?;
        self.bind_auxiliary_accounting(proof, context, purpose, requested, selected, Arc::from(provider))
    }

    pub(crate) fn smart_group_accounting(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        classifier_provider: &str,
        classifier_model: &str,
    ) -> anyhow::Result<Arc<AuxiliaryAccounting>> {
        let (requested, selected) = self.resolve_auxiliary_target(
            proof,
            context,
            AuxiliaryPurpose::SmartGroupClassifier,
            Some((classifier_provider, classifier_model)),
        )?;
        let provider = crate::providers::create_provider_for(
            &selected,
            &self.provider_catalog,
            &self.generation.effective.reliability,
            &self.generation.effective.cost,
            &crate::providers::provider_runtime_options_from_config(&self.generation.effective),
        )
        .map_err(|_| anyhow::anyhow!("smart-group classifier provider construction refused"))?;
        self.bind_auxiliary_accounting(
            proof,
            context,
            AuxiliaryPurpose::SmartGroupClassifier,
            requested,
            selected,
            Arc::from(provider),
        )
    }

    fn resolve_auxiliary_target(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        purpose: AuxiliaryPurpose,
        smart_group_selection: Option<(&str, &str)>,
    ) -> anyhow::Result<((String, String), ResolvedModel)> {
        self.agent_execution_specs(proof, context)?;
        anyhow::ensure!(
            matches!(context.disposition()?, TurnDisposition::Run),
            "auxiliary call requires a running decision"
        );
        #[cfg(test)]
        crate::causal_tree::logical_clock::visit_site("auxiliary_deadline");
        anyhow::ensure!(
            crate::causal_tree::logical_clock::now() < context.engine.deadline,
            "auxiliary decision deadline expired"
        );
        let original = self
            .resolve_agent_model(proof)
            .map_err(|_| anyhow::anyhow!("auxiliary source model resolution refused"))?;
        let requested = match purpose {
            AuxiliaryPurpose::SignalVision => (proof.provider_name().to_owned(), proof.model_name().to_owned()),
            AuxiliaryPurpose::SmartGroupClassifier => {
                let (provider, model) = smart_group_selection
                    .ok_or_else(|| anyhow::anyhow!("smart-group classifier selection is missing"))?;
                (
                    if provider.trim().is_empty() {
                        original.provider_label().to_owned()
                    } else {
                        provider.trim().to_owned()
                    },
                    if model.trim().is_empty() {
                        original.model().to_owned()
                    } else {
                        model.trim().to_owned()
                    },
                )
            }
        };
        let selected = match purpose {
            AuxiliaryPurpose::SignalVision => original,
            AuxiliaryPurpose::SmartGroupClassifier => {
                if requested.0 == original.provider_label() && requested.1 == original.model() {
                    original
                } else {
                    let resolve = |provider: &str, model: &str| {
                        let credential = if crate::providers::ProviderId::of(provider) == *original.provider() {
                            self.credential_for_proof(proof)
                        } else {
                            crate::providers::selection::credential_for(&self.generation.effective, provider)
                        };
                        crate::providers::selection::check_with(
                            &self.generation.effective,
                            &self.provider_catalog,
                            provider,
                            model,
                            credential,
                            &Requirement::chat(),
                            UnknownPolicy::Invocation,
                        )
                    };
                    match resolve(&requested.0, &requested.1) {
                        Ok(selected) => selected,
                        Err(_) if requested.0 != original.provider_label() => {
                            let fallback = (original.provider_label().to_owned(), requested.1.clone());
                            let selected = resolve(&fallback.0, &fallback.1)
                                .map_err(|_| anyhow::anyhow!("smart-group classifier model is unavailable"))?;
                            anyhow::ensure!(
                                self.child_scope().is_none() || proof.model_binding().matches(&selected),
                                "smart-group classifier selection exceeds child model scope"
                            );
                            return Ok((fallback, selected));
                        }
                        Err(_) => return Err(anyhow::anyhow!("smart-group classifier model is unavailable")),
                    }
                }
            }
        };
        anyhow::ensure!(
            self.child_scope().is_none() || proof.model_binding().matches(&selected),
            "auxiliary selection exceeds child model scope"
        );
        Ok((requested, selected))
    }

    fn bind_auxiliary_accounting(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        purpose: AuxiliaryPurpose,
        requested: (String, String),
        selected: ResolvedModel,
        provider: Arc<dyn crate::providers::Provider>,
    ) -> anyhow::Result<Arc<AuxiliaryAccounting>> {
        anyhow::ensure!(
            !self.generation.effective.causal_tree.enabled || (self.durable_key && self.recordable),
            "enabled auxiliary accounting requires durable source recording"
        );
        let cleanup = crate::runtime::turn_cleanup::current_tracker()
            .ok_or_else(|| anyhow::anyhow!("auxiliary call has no cleanup owner"))?;
        let scope = context.accounting.as_ref().map_or_else(
            || {
                let scope_id = self.state_key.hmac(
                    crate::decision::KeyDomain::TerminalClaim,
                    &[
                        b"turn-decision-accounting-v1",
                        proof.workspace_id().as_bytes(),
                        proof.owner_id().as_bytes(),
                        proof.session_key().as_bytes(),
                        proof.run_id().as_bytes(),
                        proof.source_event_id().unwrap_or("").as_bytes(),
                        proof.config_source_revision().as_bytes(),
                    ],
                );
                Arc::new(
                    crate::runtime::decision_accounting::AccountingScope::new(
                        Arc::clone(&self.generation.effective),
                        Arc::clone(&self.state_key),
                        cleanup,
                        scope_id,
                        context.engine.state.trusted.parts().budgets,
                        context.engine.deadline,
                    )
                    .with_money(
                        context
                            .recursive_allowance
                            .get()
                            .and_then(|owner| owner.funded_money())
                            .or_else(|| self.recursive_money(proof)),
                    ),
                )
            },
            Arc::clone,
        );
        AuxiliaryAccounting::new(
            selected,
            requested,
            provider,
            Arc::clone(&self.state_key),
            scope,
            purpose,
            context.execution_trace.get_or_init(Default::default).clone(),
        )
    }
}
