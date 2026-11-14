//! Concrete auxiliary target selection after actual source/context admission.
use crate::providers::catalog::ResolvedModel;
use crate::providers::catalog::{Requirement, UnknownPolicy};
use crate::runtime::auxiliary_call::{AuxiliaryAccounting, AuxiliaryPurpose};
use crate::runtime::turn_decision::{TurnDecisionContext, TurnDecisionService};
use crate::runtime::turn_source::TurnSourceProof;
use std::sync::Arc;

/// Auxiliary call accounting bound to the turn's admitted source (R25).
pub(crate) fn auxiliary_accounting(
    service: &TurnDecisionService,
    proof: &dyn TurnSourceProof,
    context: &TurnDecisionContext,
    purpose: AuxiliaryPurpose,
) -> anyhow::Result<Arc<AuxiliaryAccounting>> {
    anyhow::ensure!(
        purpose != AuxiliaryPurpose::SmartGroupClassifier,
        "smart-group classifier requires its configured selection"
    );
    let (requested, selected) = resolve_auxiliary_target(service, proof, context, purpose, None)?;
    let provider = crate::providers::create_provider_for(
        &selected,
        &service.provider_catalog,
        &service.generation.effective.reliability,
        &service.generation.effective.cost,
        &crate::providers::provider_runtime_options_from_config(&service.generation.effective),
    )
    .map_err(|_| anyhow::anyhow!("auxiliary provider construction refused"))?;
    bind_auxiliary_accounting(
        service,
        proof,
        context,
        purpose,
        requested,
        selected,
        Arc::from(provider),
    )
}

pub(crate) fn smart_group_accounting(
    service: &TurnDecisionService,
    proof: &dyn TurnSourceProof,
    context: &TurnDecisionContext,
    classifier_provider: &str,
    classifier_model: &str,
) -> anyhow::Result<Arc<AuxiliaryAccounting>> {
    let (requested, selected) = resolve_auxiliary_target(
        service,
        proof,
        context,
        AuxiliaryPurpose::SmartGroupClassifier,
        Some((classifier_provider, classifier_model)),
    )?;
    let provider = crate::providers::create_provider_for(
        &selected,
        &service.provider_catalog,
        &service.generation.effective.reliability,
        &service.generation.effective.cost,
        &crate::providers::provider_runtime_options_from_config(&service.generation.effective),
    )
    .map_err(|_| anyhow::anyhow!("smart-group classifier provider construction refused"))?;
    bind_auxiliary_accounting(
        service,
        proof,
        context,
        AuxiliaryPurpose::SmartGroupClassifier,
        requested,
        selected,
        Arc::from(provider),
    )
}

fn resolve_auxiliary_target(
    service: &TurnDecisionService,
    proof: &dyn TurnSourceProof,
    context: &TurnDecisionContext,
    purpose: AuxiliaryPurpose,
    smart_group_selection: Option<(&str, &str)>,
) -> anyhow::Result<((String, String), ResolvedModel)> {
    service.agent_execution_specs(proof, context)?;
    anyhow::ensure!(context.is_run(), "auxiliary call requires a running decision");
    #[cfg(test)]
    crate::runtime::logical_clock::visit_site("auxiliary_deadline");
    // Minted from `logical_clock::now()`. `Instant::now()` ignores a logical
    // advance at this site (see the accounted skip test).
    anyhow::ensure!(
        crate::runtime::logical_clock::now() < context.engine.deadline,
        "auxiliary decision deadline expired"
    );
    let original = service
        .resolve_agent_model(proof)
        .map_err(|_| anyhow::anyhow!("auxiliary source model resolution refused"))?;
    let requested = match purpose {
        AuxiliaryPurpose::SignalVision => (proof.provider_name().to_owned(), proof.model_name().to_owned()),
        AuxiliaryPurpose::SmartGroupClassifier => {
            let (provider, model) =
                smart_group_selection.ok_or_else(|| anyhow::anyhow!("smart-group classifier selection is missing"))?;
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
                        service.credential_for_proof(proof)
                    } else {
                        crate::providers::selection::credential_for(&service.generation.effective, provider)
                    };
                    crate::providers::selection::check_with(
                        &service.generation.effective,
                        &service.provider_catalog,
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
                            service.child_scope().is_none() || proof.model_binding().matches(&selected),
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
        service.child_scope().is_none() || proof.model_binding().matches(&selected),
        "auxiliary selection exceeds child model scope"
    );
    Ok((requested, selected))
}

fn bind_auxiliary_accounting(
    service: &TurnDecisionService,
    proof: &dyn TurnSourceProof,
    context: &TurnDecisionContext,
    purpose: AuxiliaryPurpose,
    requested: (String, String),
    selected: ResolvedModel,
    provider: Arc<dyn crate::providers::Provider>,
) -> anyhow::Result<Arc<AuxiliaryAccounting>> {
    anyhow::ensure!(
        !service.generation.effective.causal_tree.enabled || (service.durable_key && service.recordable),
        "enabled auxiliary accounting requires durable source recording"
    );
    // Never pay for an auxiliary call of a turn whose main send will be
    // refused as unpriced (G6): the refusal runs before any paid call. The
    // call then reserves only what the first serving send leaves (P1-03).
    let protected = crate::agent::send_admission::protected_first_send(service, proof)?;
    let cleanup = crate::runtime::turn_cleanup::current_tracker()
        .ok_or_else(|| anyhow::anyhow!("auxiliary call has no cleanup owner"))?;
    let scope = context.runtime().accounting.as_ref().map_or_else(
        || {
            let scope_id = service.state_key.hmac(
                crate::security::runtime_key::KeyDomain::TerminalClaim,
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
                    Arc::clone(&service.generation.effective),
                    Arc::clone(&service.state_key),
                    cleanup,
                    scope_id,
                    context.engine.state.trusted.parts().budgets,
                    context.engine.deadline,
                )
                .with_money(
                    context
                        .runtime()
                        .recursive_allowance()
                        .map(|owner| owner.money())
                        .or_else(|| service.runtime.recursive_money(proof)),
                ),
            )
        },
        Arc::clone,
    );
    AuxiliaryAccounting::new(
        selected,
        requested,
        provider,
        Arc::clone(&service.state_key),
        scope,
        purpose,
        context.execution_trace.get_or_init(Default::default).clone(),
        protected,
    )
}
