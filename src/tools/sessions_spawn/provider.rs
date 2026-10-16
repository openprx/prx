use super::SessionsSpawnTool;
use crate::config::DelegateAgentConfig;
use crate::providers;
use crate::providers::Provider;
use std::sync::Arc;

/// Select a credential for a child provider.
///
/// Each credential in scope goes only to the provider identity it was issued
/// for: a named agent's key to that agent's own provider, the gateway fallback
/// to the gateway provider. A child on any other provider gets none and
/// resolves its own auth profile, or runs keyless.
pub(super) fn resolve_child_provider_api_key(
    selected_agent: Option<&DelegateAgentConfig>,
    fallback_api_key: Option<&str>,
    child_provider_name: &str,
    gateway_provider_name: &str,
) -> Option<String> {
    providers::credential_for_provider_identity(
        child_provider_name,
        &[
            (
                selected_agent.map(|agent| agent.provider.as_str()),
                selected_agent.and_then(|agent| agent.api_key.as_deref()),
            ),
            (Some(gateway_provider_name), fallback_api_key),
        ],
    )
    .map(str::to_string)
}

/// How a task-mode child gets its provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChildProvider {
    /// The spawning tool's own chain, built with the connection it was
    /// configured with (credential, base URL and rotation keys together).
    Gateway,
    /// The parent is a routed hint, so its chain is not the configured root
    /// connection even when a direct child names the root provider.
    Root,
    /// A fresh chain with this credential and no base URL.
    Build { api_key: Option<String> },
}

impl SessionsSpawnTool {
    /// Reliability settings for a sub-agent that pinned its own provider.
    ///
    /// Retry budget, backoff and per-model fallbacks carry over unchanged; the
    /// two chain-widening knobs are deliberately dropped:
    ///
    /// * `fallback_providers` — the caller named a provider on purpose (the
    ///   canonical use is "audit this with a second, different model"), so
    ///   silently serving the request from another vendor would defeat the
    ///   request while looking like success.
    /// * `api_keys` — those rotation credentials belong to the *gateway's*
    ///   provider. Replaying them against a different vendor only buys a run of
    ///   guaranteed 401s inside the retry budget.
    pub(super) fn override_reliability(base: &crate::config::ReliabilityConfig) -> crate::config::ReliabilityConfig {
        crate::config::ReliabilityConfig {
            fallback_providers: Vec::new(),
            api_keys: Vec::new(),
            ..base.clone()
        }
    }

    /// Build the provider for a sub-agent that overrides the gateway provider.
    /// The exact model and owner-bound credential have already been released by
    /// the resolver; the typed factory adds the retry and fallback policy
    /// without reopening any credential source.
    pub(super) fn build_override_provider(
        resolved: &providers::catalog::ResolvedModel,
        catalog: &providers::catalog::CatalogView,
        reliability: &crate::config::ReliabilityConfig,
        cost: &crate::config::schema::CostConfig,
        options: &providers::ProviderRuntimeOptions,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        let reliability = Self::override_reliability(reliability);
        let provider = providers::create_provider_for(resolved, catalog, &reliability, cost, options)?;
        Ok(Arc::from(provider))
    }

    /// How a task-mode child on `child_provider` gets its provider.
    ///
    /// Any spelling of the gateway's own provider identity reuses the gateway
    /// chain, which was built with the credential, base URL and rotation keys
    /// that belong together. A rebuilt chain never receives the gateway key:
    /// only a named agent's own key for the agent's own provider, or nothing.
    pub(super) fn child_provider_plan(
        &self,
        selected_agent: Option<&DelegateAgentConfig>,
        child_provider: &str,
    ) -> ChildProvider {
        if let Some(agent) = selected_agent.filter(|agent| {
            providers::ProviderId::of(&agent.provider) == providers::ProviderId::of(child_provider)
                && (agent.credential_ref.is_some()
                    || agent.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()))
        }) {
            return ChildProvider::Build {
                api_key: agent.api_key.clone(),
            };
        }
        if providers::canonical_provider_key(child_provider) == providers::canonical_provider_key(&self.provider_name) {
            if self.model.trim().starts_with("hint:") {
                return ChildProvider::Root;
            }
            return ChildProvider::Gateway;
        }
        ChildProvider::Build {
            api_key: resolve_child_provider_api_key(selected_agent, None, child_provider, &self.provider_name),
        }
    }

    #[cfg(test)]
    pub(super) fn resolve_child_compaction(
        &self,
        provider: &str,
        model: &str,
    ) -> crate::router::context::EffectiveCompactionConfig {
        self.compaction_resolver.resolve(provider, model)
    }
}
