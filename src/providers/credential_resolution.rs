//! Provider credential resolution from config and auth profiles.

#[cfg(test)]
use super::credential_resolution_observation;
use super::{
    AuthProfilesStore, MINIMAX_OAUTH_REFRESH_TOKEN_ENV, ProviderId, ProviderNamespace, ProviderRuntimeOptions,
    canonical_auth_profile_provider, gemini, is_claude_code_alias, is_claude_code_oauth_placeholder,
    is_claude_code_oauth_setup_token, is_minimax_alias, is_minimax_oauth_placeholder, is_qwen_oauth_alias,
    read_non_empty_env, resolve_claude_code_context, resolve_minimax_oauth_refresh_token, resolve_qwen_oauth_context,
    select_profile_id,
};

/// Resolve API key for a provider from explicit config only.
///
/// Resolution order:
/// 1. Explicitly provided `api_key` parameter (trimmed, filtered if empty)
/// 2. For MiniMax OAuth placeholders: MiniMax OAuth refresh-token flow
///
/// Environment variables are NOT read here. All credentials must come from
/// the config file or auth-profiles.json.
#[cfg(test)]
pub(super) fn resolve_provider_credential(name: &str, credential_override: Option<&str>) -> Option<String> {
    resolve_provider_credential_with_options(name, credential_override, &ProviderRuntimeOptions::default())
}

pub(super) fn resolve_provider_credential_with_options(
    name: &str,
    credential_override: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> Option<String> {
    if let Some(raw_override) = credential_override {
        let trimmed_override = raw_override.trim();
        if !trimmed_override.is_empty() {
            if is_minimax_alias(name) && is_minimax_oauth_placeholder(trimmed_override) {
                if let Some(credential) = resolve_minimax_oauth_refresh_token(name) {
                    return Some(credential);
                }
                return None;
            }
            return Some(trimmed_override.to_owned());
        }
    }

    // Bedrock uses AWS AKSK credentials, not a single API key.
    // Credential resolution happens inside BedrockProvider.
    if matches!(name, "bedrock" | "aws-bedrock") {
        return None;
    }

    if let Some(credential) = resolve_auth_profile_credential(name, options) {
        return Some(credential);
    }

    None
}

#[derive(Clone, Default)]
pub(super) struct ResolvedProviderCredentials {
    pub(super) credential: Option<String>,
    pub(super) qwen_base_url: Option<String>,
    pub(super) claude_refresh_token: Option<String>,
    pub(super) claude_expires_at: Option<i64>,
    /// An OAuth refresh was due and [`OauthRefresh::Never`] withheld it.
    pub(super) refresh_withheld: bool,
}

impl ResolvedProviderCredentials {
    pub(super) fn is_usable_for(&self, provider: &str) -> bool {
        if matches!(provider, "bedrock" | "aws-bedrock") {
            return true;
        }
        if matches!(provider, "gemini" | "google" | "google-gemini") {
            return self.credential.is_some() || gemini::GeminiProvider::has_any_auth();
        }
        self.credential.as_deref().is_some_and(|credential| {
            !is_claude_code_oauth_setup_token(credential) || self.claude_refresh_token.is_some()
        })
    }
}

/// Whether a credential lookup may refresh a due OAuth token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OauthRefresh {
    /// Construction: a due token is refreshed (a request to the provider's
    /// token endpoint; for Claude Code, a rewrite of its credential file).
    Allowed,
    /// A probe: nothing is refreshed and nothing is written; a due refresh
    /// is reported instead (D-080 T-7b).
    Never,
}

/// Sole provider credential resolver used by construction and availability.
/// It preserves provider-specific OAuth metadata while applying the common
/// explicit/config-profile precedence exactly once.
pub(super) fn resolve_provider_credentials(
    name: &str,
    credential_override: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> ResolvedProviderCredentials {
    resolve_provider_credentials_with(name, credential_override, options, OauthRefresh::Allowed)
}

/// [`resolve_provider_credentials`] under an explicit refresh policy.
pub(super) fn resolve_provider_credentials_with(
    name: &str,
    credential_override: Option<&str>,
    options: &ProviderRuntimeOptions,
    refresh: OauthRefresh,
) -> ResolvedProviderCredentials {
    #[cfg(test)]
    credential_resolution_observation::record(0);
    if is_qwen_oauth_alias(name) {
        let context = resolve_qwen_oauth_context(credential_override, refresh);
        return ResolvedProviderCredentials {
            credential: context.credential,
            qwen_base_url: context.base_url,
            refresh_withheld: context.refresh_withheld,
            ..ResolvedProviderCredentials::default()
        };
    }

    // A MiniMax OAuth placeholder is exchanged for an access token on every
    // construction: that exchange is a refresh.
    if refresh == OauthRefresh::Never
        && is_minimax_alias(name)
        && credential_override
            .map(str::trim)
            .is_some_and(is_minimax_oauth_placeholder)
    {
        return ResolvedProviderCredentials {
            refresh_withheld: read_non_empty_env(MINIMAX_OAUTH_REFRESH_TOKEN_ENV).is_some(),
            ..ResolvedProviderCredentials::default()
        };
    }

    let placeholder_requested = credential_override
        .map(str::trim)
        .is_some_and(is_claude_code_oauth_placeholder);
    if is_claude_code_alias(name) || (name == "anthropic" && (credential_override.is_none() || placeholder_requested)) {
        let context = resolve_claude_code_context(credential_override, refresh);
        let fallback_override = (!placeholder_requested).then_some(credential_override).flatten();
        let fallback_name = if is_claude_code_alias(name) { "anthropic" } else { name };
        return ResolvedProviderCredentials {
            credential: context
                .credential
                .or_else(|| resolve_provider_credential_with_options(fallback_name, fallback_override, options)),
            claude_refresh_token: context.refresh_token,
            claude_expires_at: context.expires_at,
            refresh_withheld: context.refresh_withheld,
            ..ResolvedProviderCredentials::default()
        };
    }

    ResolvedProviderCredentials {
        credential: resolve_provider_credential_with_options(name, credential_override, options),
        ..ResolvedProviderCredentials::default()
    }
}

/// What a connection test may present for a provider.
pub(crate) enum ProbeCredential {
    /// The credential construction would present (`None`: it has none).
    Ready(Option<String>),
    /// Only an OAuth refresh would produce the credential construction
    /// presents, and a probe does not refresh.
    RefreshDue,
}

/// The credential provider construction presents for `name` (the explicit,
/// resolver-released credential, else the provider's own store lookup, with
/// every provider-specific OAuth context), looked up without side effects:
/// no OAuth token is refreshed and no credential file is written (D-078 T-7,
/// D-080 T-7b). It reads files and takes the auth-profile lock, so it
/// blocks: call it off the async runtime.
pub(crate) fn probe_credential(
    name: &str,
    credential_override: Option<&str>,
    options: &ProviderRuntimeOptions,
) -> ProbeCredential {
    let resolved = resolve_provider_credentials_with(name, credential_override, options, OauthRefresh::Never);
    if resolved.refresh_withheld {
        ProbeCredential::RefreshDue
    } else {
        ProbeCredential::Ready(resolved.credential)
    }
}

pub(super) fn resolve_auth_profile_credential(name: &str, options: &ProviderRuntimeOptions) -> Option<String> {
    #[cfg(test)]
    credential_resolution_observation::record(1);
    let openprx_dir = options.openprx_dir.as_ref()?;
    let store = AuthProfilesStore::new(openprx_dir, options.secrets_encrypt);
    let data = store.load_read_only().ok()?;
    let profile_id = auth_profile_keys(name)
        .iter()
        .find_map(|provider| select_profile_id(&data, provider, options.auth_profile_override.as_deref()))?;
    let profile = data.profiles.get(&profile_id)?;
    profile
        .token
        .as_deref()
        .or_else(|| profile.token_set.as_ref().map(|tokens| tokens.access_token.as_str()))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
}

/// The auth-profile provider keys a provider's own credential is read under,
/// in order. A `custom:`, `anthropic-custom:` or `wasm:` identity is
/// case-sensitive, so its token is stored under the `ProviderId` key
/// (`auth::normalize_provider`); a token stored lowercased before that is
/// still read, but only when lowercasing the spelling does not change its
/// identity, so another path's or plugin's token is never read.
pub(super) fn auth_profile_keys(name: &str) -> Vec<String> {
    let identity = ProviderId::of(name);
    match identity.namespace() {
        ProviderNamespace::Custom | ProviderNamespace::AnthropicCustom | ProviderNamespace::Wasm => {
            let legacy = name.trim().to_ascii_lowercase();
            let mut keys = vec![identity.as_str().to_string()];
            if ProviderId::of(&legacy) == identity && legacy != identity.as_str() {
                keys.push(legacy);
            }
            keys
        }
        ProviderNamespace::DecisionService => vec![identity.as_str().to_string()],
        ProviderNamespace::Catalog | ProviderNamespace::Unknown => vec![canonical_auth_profile_provider(name)],
    }
}
