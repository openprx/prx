//! A local, read-only answer to "would a chat turn on this provider and model
//! find a usable credential?", for listings that report readiness (the rf
//! agent catalog's `credentials_ready`).
//!
//! A turn picks its credential in one of these ways, and each is listed as a
//! candidate reference by [`selection_references`]:
//! - the provider's own selection, [`credential_for`]: the root connection or
//!   `cred-<name>` for the configured default provider, otherwise the
//!   provider's own store (a non-reference auth profile or OAuth login). A
//!   `cred-<name>` reference stored for a non-default provider is used only
//!   where something references it;
//! - a `[[model_routes]]` entry for this provider and model (`hint:<route>`):
//!   its inline `api_key`, else its `credential_ref`, else the provider's own
//!   selection (`selection::check_with`);
//! - a named agent bound to this provider and model: its `credential_ref`,
//!   else its inline `api_key`, else the provider's own selection
//!   (`gateway/api/sessions/target.rs`, `tools/delegate.rs`).
//!
//! [`reference_ready`] follows construction (`ResolvedPrimary::
//! resolve_credentials` after the resolver's release step) for one reference,
//! but never refreshes an OAuth token, never writes and sends nothing.

use super::credential_resolution::{OauthRefresh, resolve_provider_credentials_with};
use super::credentials::{ConfigCredentialSource, CredentialLookupError, CredentialRef, CredentialSource, LegacySlot};
use super::info::provider_requires_explicit_credential;
use super::selection::credential_for;
use super::{ProviderId, provider_runtime_options_from_config};
use crate::config::Config;

/// Every credential reference a chat turn on `provider`/`model` can run
/// with, without duplicates; the provider's own selection comes first.
#[must_use]
pub(crate) fn selection_references(config: &Config, provider: &str, model: &str) -> Vec<CredentialRef> {
    let identity = ProviderId::of(provider);
    let model = model.trim();
    let serves = |candidate_provider: &str, candidate_model: &str| {
        ProviderId::of(candidate_provider) == identity && candidate_model.trim().eq_ignore_ascii_case(model)
    };
    let non_empty = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let mut references = vec![credential_for(config, provider)];
    let mut push = |reference: CredentialRef| {
        if !references.contains(&reference) {
            references.push(reference);
        }
    };
    for route in config
        .model_routes
        .iter()
        .filter(|route| serves(&route.provider, &route.model))
    {
        push(if non_empty(route.api_key.as_deref()).is_some() {
            CredentialRef::LegacyInline(LegacySlot::ModelRoute(route.hint.trim().to_owned()))
        } else if let Some(id) = non_empty(route.credential_ref.as_deref()) {
            CredentialRef::Profile { id }
        } else {
            credential_for(config, &route.provider)
        });
    }
    for (name, agent) in config
        .agents
        .iter()
        .filter(|(_, agent)| serves(&agent.provider, &agent.model))
    {
        push(non_empty(agent.credential_ref.as_deref()).map_or_else(
            || {
                if non_empty(agent.api_key.as_deref()).is_some() {
                    CredentialRef::LegacyInline(LegacySlot::Agent(name.clone()))
                } else {
                    credential_for(config, &agent.provider)
                }
            },
            |id| CredentialRef::Profile { id },
        ));
    }
    references
}

/// Whether `provider` gets a usable credential through `reference`.
///
/// `true` when the provider needs no explicit credential (a local runtime, an
/// OpenAI-compatible custom endpoint, Bedrock with ambient credentials), or
/// when the reference resolves to a usable credential issued for this
/// provider; an OAuth credential whose refresh is due counts, because it
/// refreshes on use. `false` when a credential is required and the reference
/// yields none for this provider, including one issued for another provider.
///
/// # Errors
///
/// The credential store could not be read, or a reference is ambiguous.
pub(crate) fn reference_ready(
    config: &Config,
    provider: &str,
    reference: &CredentialRef,
) -> Result<bool, CredentialLookupError> {
    let name = provider.trim();
    if !provider_requires_explicit_credential(name) {
        return Ok(true);
    }
    let explicit = match reference {
        CredentialRef::None => return Ok(false),
        CredentialRef::ProviderStore => None,
        CredentialRef::Profile { .. } | CredentialRef::LegacyInline(_) => {
            let source = ConfigCredentialSource::new(config);
            if source.owner_of(reference)? != Some(ProviderId::of(name)) {
                return Ok(false);
            }
            let Some(material) = source.material(reference)?.filter(|material| !material.is_empty()) else {
                return Ok(false);
            };
            match material.api_key() {
                Some(key) => Some(key.to_owned()),
                // Root material with a URL or rotation keys but no key: the
                // provider's own store answers, as at construction.
                None if matches!(reference, CredentialRef::LegacyInline(LegacySlot::Root)) => None,
                None => return Ok(false),
            }
        }
    };
    let options = provider_runtime_options_from_config(config);
    let resolved = resolve_provider_credentials_with(name, explicit.as_deref(), &options, OauthRefresh::Never);
    Ok(resolved.is_usable_for(name) || resolved.refresh_withheld)
}
