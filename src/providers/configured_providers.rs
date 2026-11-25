//! Which providers this install has configured.
//!
//! A provider is configured when the operator named it somewhere durable: the
//! default provider, a fallback, a named agent, a model route, an embedding
//! route, or a stored credential. Surfaces that probe providers on their own
//! initiative (`prx doctor models`) contact only these unless the operator
//! asks for more.

use crate::auth::profiles::AuthProfilesStore;
use crate::config::Config;
use crate::providers::model_discovery::{auth_state_dir, canonical_provider_name};

/// The configured provider names, in first-seen order, without duplicates.
///
/// Names are returned as the operator spelled them; two spellings of one
/// provider (an alias and its canonical name) count once.
pub fn configured_provider_names(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        let name = raw.trim();
        if name.is_empty() {
            return;
        }
        let key = canonical_key(name);
        if !names.iter().any(|existing| canonical_key(existing) == key) {
            names.push(name.to_string());
        }
    };

    if let Some(provider) = config.default_provider.as_deref() {
        push(provider);
    }
    for provider in &config.reliability.fallback_providers {
        push(provider);
    }
    let mut agent_names: Vec<&String> = config.agents.keys().collect();
    agent_names.sort();
    for agent in agent_names {
        if let Some(agent) = config.agents.get(agent) {
            push(&agent.provider);
        }
    }
    for route in &config.model_routes {
        push(&route.provider);
    }
    for route in &config.embedding_routes {
        push(&route.provider);
    }

    let store = AuthProfilesStore::new(&auth_state_dir(config), config.secrets.encrypt);
    if let Ok(data) = store.load_read_only() {
        for profile in data.profiles.values() {
            push(&profile.provider);
        }
    }
    names
}

/// Whether `provider` is one of the configured providers (alias-insensitive).
pub fn is_configured(configured: &[String], provider: &str) -> bool {
    let key = canonical_key(provider);
    configured.iter().any(|name| canonical_key(name) == key)
}

fn canonical_key(name: &str) -> String {
    let lowered = name.trim().to_ascii_lowercase();
    canonical_provider_name(&lowered).to_string()
}
