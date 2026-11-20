//! A local, read-only answer to "would a chat provider for this provider be
//! handed a usable credential?", for listings that report readiness (the rf
//! agent catalog's `credentials_ready`).
//!
//! It follows construction (`ResolvedPrimary::resolve_credentials` after the
//! resolver's release step) with the credential [`credential_for`] picks, but
//! never refreshes an OAuth token, never writes and sends nothing.

use super::credential_resolution::{OauthRefresh, resolve_provider_credentials_with};
use super::credentials::{ConfigCredentialSource, CredentialLookupError, CredentialRef, CredentialSource, LegacySlot};
use super::info::provider_requires_explicit_credential;
use super::selection::credential_for;
use super::{ProviderId, provider_runtime_options_from_config};
use crate::config::Config;

/// Whether `provider` has a usable credential.
///
/// `true` when the provider needs no explicit credential (a local runtime, an
/// OpenAI-compatible custom endpoint, Bedrock with ambient credentials), or
/// when a credential resolves for it; an OAuth credential whose refresh is due
/// counts, because it refreshes on use. `false` when a credential is required
/// and none is stored for this provider, including a reference issued for
/// another provider.
///
/// # Errors
///
/// The credential store could not be read, or a reference is ambiguous.
pub(crate) fn credential_ready(config: &Config, provider: &str) -> Result<bool, CredentialLookupError> {
    let name = provider.trim();
    if !provider_requires_explicit_credential(name) {
        return Ok(true);
    }
    let reference = credential_for(config, name);
    let explicit = match &reference {
        CredentialRef::None => return Ok(false),
        CredentialRef::ProviderStore => None,
        CredentialRef::Profile { .. } | CredentialRef::LegacyInline(_) => {
            let source = ConfigCredentialSource::new(config);
            if source.owner_of(&reference)? != Some(ProviderId::of(name)) {
                return Ok(false);
            }
            let Some(material) = source.material(&reference)?.filter(|material| !material.is_empty()) else {
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
