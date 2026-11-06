//! Model admission owned by the ingress binding, without decision state.
use crate::providers::catalog::{CatalogView, Feature, Requirement, UnknownPolicy, Verification};
use crate::runtime::turn_source::TurnSourceProof;

pub(crate) fn validate_execution(proof: &dyn TurnSourceProof, provider: &str, model: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        proof
            .model_binding()
            .admits_execution((proof.provider_name(), proof.model_name()), (provider, model),),
        "execution model differs from the ingress model binding"
    );
    Ok(())
}

pub(crate) fn can_call_tools(
    config: &crate::config::Config,
    catalog: &CatalogView,
    proof: &dyn TurnSourceProof,
    credential: crate::providers::credentials::CredentialRef,
) -> bool {
    proof.tool_calls_allowed()
        && crate::providers::selection::check_with(
            config,
            catalog,
            proof.provider_name(),
            proof.model_name(),
            credential,
            &Requirement::chat().with(Feature::NativeTools),
            UnknownPolicy::Invocation,
        )
        .is_ok_and(|resolved| {
            resolved.verification() == &Verification::Verified && proof.model_binding().matches(&resolved)
        })
}
