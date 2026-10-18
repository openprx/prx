use super::SessionsSpawnTool;
use crate::config::DelegateAgentConfig;
use crate::providers;

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

impl SessionsSpawnTool {
    /// Whether an ordinary child request names the already-admitted parent
    /// connection. A named agent with its own credential and a routed parent
    /// both require their separately admitted binding.
    pub(super) fn reuses_parent_provider_identity(
        &self,
        selected_agent: Option<&DelegateAgentConfig>,
        child_provider: &str,
    ) -> bool {
        if selected_agent.is_some_and(|agent| {
            providers::ProviderId::of(&agent.provider) == providers::ProviderId::of(child_provider)
                && (agent.credential_ref.is_some()
                    || agent.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()))
        }) {
            return false;
        }
        providers::canonical_provider_key(child_provider) == providers::canonical_provider_key(&self.provider_name)
            && !self.model.trim().starts_with("hint:")
    }
}
