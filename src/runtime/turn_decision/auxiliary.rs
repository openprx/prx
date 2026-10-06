//! Concrete auxiliary target selection after actual source/context admission.
use super::{TurnDecisionContext, TurnDecisionService, TurnDisposition, TurnSourceProof};
use crate::providers::catalog::ResolvedModel;
#[cfg(feature = "llm-router")]
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
        #[cfg(feature = "llm-router")]
        anyhow::ensure!(
            purpose != AuxiliaryPurpose::AutomixPremium,
            "Automix requires a settled main continuation"
        );
        let (requested, selected) = self.resolve_auxiliary_target(proof, context, purpose, None)?;
        let provider = crate::providers::create_provider_for(
            &selected,
            &self.provider_catalog,
            &self.generation.effective.reliability,
            &crate::providers::provider_runtime_options_from_config(&self.generation.effective),
        )
        .map_err(|_| anyhow::anyhow!("auxiliary provider construction refused"))?;
        self.bind_auxiliary_accounting(proof, context, purpose, requested, selected, Arc::from(provider))
    }

    #[cfg(feature = "llm-router")]
    pub(crate) fn automix_requested_selection(
        &self,
        continuation: &crate::runtime::turn_terminal::SettledAutomixContinuation,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<(String, String)> {
        self.resolve_auxiliary_target(proof, context, AuxiliaryPurpose::AutomixPremium, Some(continuation))
            .map(|(requested, _)| requested)
    }

    #[cfg(feature = "llm-router")]
    pub(crate) fn automix_accounting_with_selection(
        &self,
        continuation: crate::runtime::turn_terminal::SettledAutomixContinuation,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        selection: &crate::agent::embedded_source::SelectedEmbeddedModel,
    ) -> anyhow::Result<Arc<AuxiliaryAccounting>> {
        let purpose = AuxiliaryPurpose::AutomixPremium;
        let (requested, selected) = self.resolve_auxiliary_target(proof, context, purpose, Some(&continuation))?;
        anyhow::ensure!(
            selection.choice().binding().matches(&selected),
            "auxiliary registered provider selection differs"
        );
        self.bind_auxiliary_accounting(proof, context, purpose, requested, selected, selection.provider())
    }

    fn resolve_auxiliary_target(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        purpose: AuxiliaryPurpose,
        #[cfg(feature = "llm-router")] continuation: Option<&crate::runtime::turn_terminal::SettledAutomixContinuation>,
        #[cfg(not(feature = "llm-router"))] _continuation: Option<&()>,
    ) -> anyhow::Result<((String, String), ResolvedModel)> {
        self.agent_execution_specs(proof, context)?;
        match purpose {
            AuxiliaryPurpose::SignalVision => anyhow::ensure!(
                matches!(context.disposition()?, TurnDisposition::Run),
                "auxiliary call requires a running decision"
            ),
            #[cfg(feature = "llm-router")]
            AuxiliaryPurpose::AutomixPremium => continuation
                .ok_or_else(|| anyhow::anyhow!("Automix requires a settled main continuation"))?
                .verify(self, proof, context)?,
        }
        anyhow::ensure!(
            std::time::Instant::now() < context.engine.deadline,
            "auxiliary decision deadline expired"
        );
        let original = self
            .resolve_agent_model(proof)
            .map_err(|_| anyhow::anyhow!("auxiliary source model resolution refused"))?;
        let requested = match purpose {
            AuxiliaryPurpose::SignalVision => (proof.provider_name().to_owned(), proof.model_name().to_owned()),
            #[cfg(feature = "llm-router")]
            AuxiliaryPurpose::AutomixPremium => premium_selection(&self.generation.effective, proof.provider_name())?,
        };
        let selected = match purpose {
            AuxiliaryPurpose::SignalVision => original,
            #[cfg(feature = "llm-router")]
            AuxiliaryPurpose::AutomixPremium => {
                let credential = if crate::providers::ProviderId::of(&requested.0) == *original.provider() {
                    self.credential_for_proof(proof)
                } else {
                    crate::providers::selection::credential_for(&self.generation.effective, &requested.0)
                };
                let selected = crate::providers::selection::check_with(
                    &self.generation.effective,
                    &self.provider_catalog,
                    &requested.0,
                    &requested.1,
                    credential,
                    &Requirement::chat(),
                    UnknownPolicy::Invocation,
                )
                .map_err(|_| anyhow::anyhow!("auxiliary configured premium model is unavailable"))?;
                anyhow::ensure!(
                    self.child_scope().is_none() || proof.model_binding().matches(&selected),
                    "auxiliary premium selection exceeds child model scope"
                );
                selected
            }
        };
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
                Arc::new(crate::runtime::decision_accounting::AccountingScope::new(
                    Arc::clone(&self.generation.effective),
                    Arc::clone(&self.state_key),
                    cleanup,
                    scope_id,
                    context.engine.state.trusted.parts().budgets,
                    context.engine.deadline,
                ))
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
        )
    }
}

#[cfg(any(test, feature = "llm-router"))]
fn premium_selection(config: &crate::config::Config, admitted_provider: &str) -> anyhow::Result<(String, String)> {
    let premium = config.router.automix.premium_model_id.trim();
    anyhow::ensure!(!premium.is_empty(), "auxiliary premium model is not configured");
    if premium.starts_with("hint:") {
        return Ok((admitted_provider.to_owned(), premium.to_owned()));
    }
    let mut selected = None;
    for entry in &config.router.models {
        let matches = premium == entry.model_id
            || premium
                .strip_prefix(entry.provider.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                == Some(entry.model_id.as_str());
        if !matches {
            continue;
        }
        let pair = (entry.provider.clone(), entry.model_id.clone());
        anyhow::ensure!(
            selected.as_ref().is_none_or(|prior| prior == &pair),
            "auxiliary premium selection is ambiguous"
        );
        selected = Some(pair);
    }
    if let Some(selected) = selected {
        return Ok(selected);
    }
    if config.model_routes.iter().any(|route| route.hint == premium) {
        return Ok((admitted_provider.to_owned(), format!("hint:{premium}")));
    }
    if let Some((provider, model)) = premium.split_once('/') {
        if !provider.contains(':') && crate::providers::ProviderId::of(provider).is_resolvable() && !model.is_empty() {
            return Ok((provider.to_owned(), model.to_owned()));
        }
    }
    Ok((admitted_provider.to_owned(), premium.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::premium_selection;
    #[test]
    fn configured_premium_keeps_route_hint_and_bare_model_source_provider() {
        let mut config = crate::config::Config::default();
        config.router.automix.premium_model_id = "hint:premium-route".to_owned();
        assert_eq!(
            premium_selection(&config, "openai").expect("configured hint"),
            ("openai".to_owned(), "hint:premium-route".to_owned())
        );
        config.router.automix.premium_model_id = "organization/model".to_owned();
        assert_eq!(
            premium_selection(&config, "openai").expect("bare namespaced model"),
            ("openai".to_owned(), "organization/model".to_owned())
        );
    }
    #[test]
    fn configured_premium_ambiguous_registry_refuses_but_exact_pair_selects() {
        let mut config = crate::config::Config::default();
        for provider in ["openai", "openrouter"] {
            config.router.models.push(
                serde_json::from_value(serde_json::json!({"provider": provider, "model_id": "premium-model"}))
                    .expect("router config fixture"),
            );
        }
        config.router.automix.premium_model_id = "premium-model".to_owned();
        assert!(premium_selection(&config, "openai").is_err());
        config.router.automix.premium_model_id = "openrouter/premium-model".to_owned();
        assert_eq!(
            premium_selection(&config, "openai").expect("exact configured pair"),
            ("openrouter".to_owned(), "premium-model".to_owned())
        );
    }
}
