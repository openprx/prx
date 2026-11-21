use super::SessionsSpawnTool;
use crate::config::DelegateAgentConfig;
use crate::providers;

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
