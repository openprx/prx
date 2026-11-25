//! Which providers this install has configured.
//!
//! One definition, used by every surface that probes providers on its own
//! initiative (`prx doctor models`, `prx models refresh`). A provider is
//! configured when the operator gave it something to run with:
//!
//! - a credential: an inline key (the root `api_key`, a rotation key or an
//!   `api_url` for the default provider; a route or agent `api_key`), a
//!   credential reference (`cred-<name>`), or a stored auth profile (OAuth or
//!   token); or
//! - nothing, because the provider needs none: a local runtime in the catalog
//!   (Ollama, llama.cpp, vLLM, LM Studio) or a `custom:<url>` endpoint the
//!   operator wrote.
//!
//! It must also be named by the operator: the default provider, a fallback, a
//! named agent, a model route, an embedding route, or a stored profile.
//!
//! A `default_provider` that only came from the `prx init` template, with no
//! credential, is not configured. A key in the process environment is never
//! proof of configuration: it is sent only to a provider that is configured by
//! this definition or named explicitly on the command line.

use crate::auth::profiles::AuthProfilesStore;
use crate::config::Config;
use crate::providers::credentials::CredentialRef;
use crate::providers::model_discovery::{auth_state_dir, canonical_provider_name, model_discovery_api_key};

/// The configured provider names, in first-seen order, without duplicates.
///
/// Names are returned as the operator spelled them; two spellings of one
/// provider (an alias and its canonical name) count once.
pub fn configured_provider_names(config: &Config) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |raw: &str, inline_key: Option<&str>, reference: Option<&str>| {
        let name = raw.trim();
        if name.is_empty() {
            return;
        }
        let key = canonical_key(name);
        if names.iter().any(|existing| canonical_key(existing) == key) {
            return;
        }
        if has_credential_or_needs_none(config, name, inline_key, reference) {
            names.push(name.to_string());
        }
    };

    if let Some(provider) = config.default_provider.as_deref() {
        push(provider, None, None);
    }
    for provider in &config.reliability.fallback_providers {
        push(provider, None, None);
    }
    let mut agent_names: Vec<&String> = config.agents.keys().collect();
    agent_names.sort();
    for agent in agent_names {
        if let Some(agent) = config.agents.get(agent) {
            push(&agent.provider, agent.api_key.as_deref(), agent.credential_ref.as_deref());
        }
    }
    for route in &config.model_routes {
        push(&route.provider, route.api_key.as_deref(), route.credential_ref.as_deref());
    }
    for route in &config.embedding_routes {
        push(&route.provider, route.api_key.as_deref(), route.credential_ref.as_deref());
    }

    let store = AuthProfilesStore::new(&auth_state_dir(config), config.secrets.encrypt);
    if let Ok(data) = store.load_read_only() {
        for profile in data.profiles.values() {
            push(&profile.provider, None, None);
        }
    }
    names
}

/// The default provider when it is configured, else `None`: a default that
/// only came from the init template has no credential and contacts nobody.
pub fn configured_default_provider(config: &Config) -> Option<String> {
    let provider = config.default_provider.as_deref()?.trim();
    if provider.is_empty() {
        return None;
    }
    has_credential_or_needs_none(config, provider, None, None).then(|| provider.to_string())
}

/// The one line printed, by every command that would otherwise probe, when
/// nothing is configured.
pub const NO_PROVIDER_CONFIGURED: &str = "No provider is configured (no credential has been set up), so no host was contacted. \
Set one up with `prx init --provider <name> --from-stdin` or `prx auth login --provider <name>`.";

fn nonblank(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

/// Whether `name` has a usable credential of the operator's, or needs none.
/// Environment variables are not consulted.
fn has_credential_or_needs_none(config: &Config, name: &str, inline_key: Option<&str>, reference: Option<&str>) -> bool {
    if nonblank(inline_key) || nonblank(reference) {
        return true;
    }
    let canonical = canonical_provider_name(&name.to_ascii_lowercase()).to_string();
    // Bedrock is exempt from the explicit-credential rule because it may use
    // ambient AWS credentials; ambient (environment) material is not proof.
    let bedrock = matches!(canonical.as_str(), "bedrock" | "aws-bedrock");
    if !bedrock && !crate::providers::info::provider_requires_explicit_credential(name) {
        return true;
    }
    // Root material (default provider only) or a stored `cred-<name>` reference.
    if matches!(
        crate::providers::selection::credential_for(config, name),
        CredentialRef::LegacyInline(_) | CredentialRef::Profile { .. }
    ) {
        return true;
    }
    // The provider's own auth profile (OAuth or token).
    !model_discovery_api_key(config, name).is_empty()
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
