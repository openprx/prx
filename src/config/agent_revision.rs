//! Canonical non-secret agent revision metadata.
use crate::config::DelegateAgentConfig;
use serde::Serialize;

/// The non-secret fields that identify an agent's configuration, in the order
/// they are hashed. `api_key` is never part of it; the credential is named by
/// reference only.
#[derive(Serialize)]
struct RevisionPreimage<'a> {
    provider: &'a str,
    model: &'a str,
    system_prompt: Option<&'a str>,
    credential_ref: Option<&'a str>,
    temperature: Option<f64>,
    max_depth: u32,
    agentic: bool,
    allowed_tools: &'a [String],
    identity_dir: Option<&'a str>,
    memory_scope: Option<&'a str>,
    spawn_enabled: Option<bool>,
    enabled: bool,
    autonomy: Option<crate::security::AutonomyLevel>,
    limits: &'a crate::config::AgentLimits,
}

/// Lowercase SHA-256 over the serialized preimage.
pub(crate) fn config_revision(agent: &DelegateAgentConfig) -> anyhow::Result<String> {
    let preimage = serde_json::to_string(&RevisionPreimage {
        provider: &agent.provider,
        model: &agent.model,
        system_prompt: agent.system_prompt.as_deref(),
        credential_ref: agent.credential_ref.as_deref(),
        temperature: agent.temperature,
        max_depth: agent.max_depth,
        agentic: agent.agentic,
        allowed_tools: &agent.allowed_tools,
        identity_dir: agent.identity_dir.as_deref(),
        memory_scope: agent.memory_scope.as_deref(),
        spawn_enabled: agent.spawn_enabled,
        enabled: agent.enabled,
        autonomy: agent.autonomy,
        limits: &agent.limits,
    })?;
    use sha2::{Digest, Sha256};
    Ok(hex::encode(Sha256::digest(preimage.as_bytes())))
}
