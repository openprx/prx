//! Concrete auxiliary target selection after actual source/context admission.
use super::{TurnDecisionContext, TurnDecisionService, TurnDisposition, TurnSourceProof};
use crate::providers::catalog::ResolvedModel;
use crate::providers::catalog::{Requirement, UnknownPolicy};
use crate::runtime::auxiliary_call::{AuxiliaryAccounting, AuxiliaryPurpose};
use std::sync::Arc;

#[cfg(feature = "llm-router")]
impl TurnDecisionContext {
    pub(crate) fn begin_automix_window(
        &self,
        cheap_reply_at: std::time::Instant,
        window_ms: u64,
    ) -> anyhow::Result<std::time::Instant> {
        anyhow::ensure!((1..=120_000).contains(&window_ms), "invalid Automix escalation window");
        Ok(*self
            .automix_deadline
            .get_or_init(|| cheap_reply_at + std::time::Duration::from_millis(window_ms)))
    }

    pub(crate) fn automix_deadline(&self) -> anyhow::Result<std::time::Instant> {
        self.automix_deadline
            .get()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Automix window has not started"))
    }
}

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
        anyhow::ensure!(
            purpose != AuxiliaryPurpose::SmartGroupClassifier,
            "smart-group classifier requires its configured selection"
        );
        let (requested, selected) = self.resolve_auxiliary_target(proof, context, purpose, None, None)?;
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
            None,
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

    #[cfg(feature = "llm-router")]
    pub(crate) fn automix_requested_selection(
        &self,
        continuation: &crate::runtime::turn_terminal::SettledAutomixContinuation,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<(String, String)> {
        self.resolve_auxiliary_target(
            proof,
            context,
            AuxiliaryPurpose::AutomixPremium,
            Some(continuation),
            None,
        )
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
        let (requested, selected) =
            self.resolve_auxiliary_target(proof, context, purpose, Some(&continuation), None)?;
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
        smart_group_selection: Option<(&str, &str)>,
    ) -> anyhow::Result<((String, String), ResolvedModel)> {
        self.agent_execution_specs(proof, context)?;
        match purpose {
            AuxiliaryPurpose::SignalVision | AuxiliaryPurpose::SmartGroupClassifier => anyhow::ensure!(
                matches!(context.disposition()?, TurnDisposition::Run),
                "auxiliary call requires a running decision"
            ),
            #[cfg(feature = "llm-router")]
            AuxiliaryPurpose::AutomixPremium => continuation
                .ok_or_else(|| anyhow::anyhow!("Automix requires a settled main continuation"))?
                .verify(self, proof, context)?,
        }
        let deadline = match purpose {
            #[cfg(feature = "llm-router")]
            AuxiliaryPurpose::AutomixPremium => context.automix_deadline()?,
            _ => context.engine.deadline,
        };
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
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
            #[cfg(feature = "llm-router")]
            AuxiliaryPurpose::AutomixPremium => premium_selection(&self.generation.effective, proof.provider_name())?,
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
        #[cfg(feature = "llm-router")]
        let scope = if purpose == AuxiliaryPurpose::AutomixPremium {
            Arc::new(scope.automix_window(context.automix_deadline()?))
        } else {
            scope
        };
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
    #[cfg(feature = "llm-router")]
    #[tokio::test]
    async fn expired_premium_window_cannot_restart_or_change_the_decision_deadline() -> anyhow::Result<()> {
        use super::super::{AgentTurnKind, tests::disabled_agent_service};
        use std::time::{Duration, Instant};
        let workspace = tempfile::tempdir()?;
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.causal_tree.enabled = false;
        let generation = crate::config::new_shared(config.clone()).pin();
        let service = disabled_agent_service(
            &generation,
            &crate::providers::catalog::CatalogView::from_config(&config),
        )
        .await;
        let source = crate::agent::entry::test_cli_agent_turn_proof_with_input(
            &generation,
            "ollama",
            "prx-local-model",
            "Explain `2 + 2`.",
        );
        let context = service
            .decide_agent_initial(
                &source,
                AgentTurnKind::SingleShot,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await?;
        let decision_deadline = context.engine.deadline;
        let reply_at = Instant::now()
            .checked_sub(Duration::from_millis(650))
            .ok_or_else(|| anyhow::anyhow!("fixture monotonic clock cannot represent the cheap reply"))?;
        let expired = context.begin_automix_window(reply_at, 100)?;
        assert_eq!(expired, reply_at + Duration::from_millis(100));
        assert!(expired < Instant::now());
        assert_eq!(context.begin_automix_window(Instant::now(), 30_000)?, expired);
        assert_eq!(context.engine.deadline, decision_deadline);
        assert!(context.begin_automix_window(Instant::now(), 120_001).is_err());
        Ok(())
    }

    #[test]
    fn automix_legacy_configuration_gets_bounded_default_and_rejects_invalid_windows() -> anyhow::Result<()> {
        let mut config: crate::config::AutomixConfig = serde_json::from_str("{}")?;
        assert_eq!(config.escalation_window_ms, 30_000);
        config.validate()?;
        for invalid in [0, 120_001, u64::MAX] {
            config.escalation_window_ms = invalid;
            assert!(config.validate().is_err());
        }
        config.escalation_window_ms = 120_000;
        config.validate()?;
        Ok(())
    }

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
