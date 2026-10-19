#![cfg(test)]

use super::resolve_child_provider_api_key;
use crate::config::DelegateAgentConfig;

fn kimi_agent(api_key: Option<&str>) -> DelegateAgentConfig {
    DelegateAgentConfig {
        provider: "kimi-code".to_string(),
        model: "k3".to_string(),
        system_prompt: None,
        api_key: api_key.map(str::to_string),
        credential_ref: None,
        temperature: None,
        max_depth: 3,
        agentic: false,
        allowed_tools: Vec::new(),
        identity_dir: None,
        memory_scope: None,
        spawn_enabled: None,
    }
}

#[test]
fn gateway_fallback_is_not_forwarded_to_an_overridden_provider() {
    assert_eq!(
        resolve_child_provider_api_key(None, Some("qwen-key"), "kimi-code", "custom:http://127.0.0.1:18082/v1",),
        None
    );
    let agent = kimi_agent(Some("kimi-key"));
    assert_eq!(
        resolve_child_provider_api_key(
            Some(&agent),
            Some("qwen-key"),
            "kimi-code",
            "custom:http://127.0.0.1:18082/v1",
        ),
        Some("kimi-key".to_string())
    );
    assert_eq!(
        resolve_child_provider_api_key(
            None,
            Some("qwen-key"),
            "custom:http://127.0.0.1:18082/v1",
            "custom:http://127.0.0.1:18082/v1",
        ),
        Some("qwen-key".to_string())
    );
}

/// C0-C: a named agent's key belongs to the agent's provider; a child the
/// model moved to another provider does not carry it there.
#[test]
fn an_agent_key_is_not_forwarded_to_another_provider() {
    let agent = kimi_agent(Some("kimi-key"));
    assert_eq!(
        resolve_child_provider_api_key(Some(&agent), None, "custom:https://elsewhere.example/v1", "openai"),
        None
    );
    assert_eq!(
        resolve_child_provider_api_key(Some(&agent), Some("root-key"), "OpenAI", "openai"),
        Some("root-key".to_string())
    );
}
