//! Owner-bound credential references (PMM-D-6).
//!
//! A credential reference is an entry of the encrypted auth-profile store
//! (`auth-profiles.json` next to `config.toml`, the same store OAuth and
//! token profiles already use) with an opaque id `cred-<name>` and an owner:
//! the provider it was issued for. A selection names it by id
//! ([`CredentialRef::Profile`]); the resolver releases its material only when
//! the owner's provider identity equals the identity the selection targets
//! (`providers::catalog::resolve`), so no reference can hand one provider's
//! credential to another. [`CredentialReferences::check_reference`] applies
//! the same rule for a configuration that stores a reference, so a reference
//! to a missing credential or to another provider's credential is refused
//! before anything is written.
//!
//! The value is write-only: [`CredentialReferences::set`] stores it
//! (encrypted when `secrets.encrypt = true`), and every other answer —
//! [`CredentialStatus`], errors, `Debug` — carries the id, the owner (URL
//! userinfo redacted) and timestamps only.
//!
//! A reference is used only by reference: the provider's own store lookup
//! (`auth::select_profile_id`) never falls back to a `cred-` entry, so creating
//! one for a route does not silently change the provider's default
//! credential.
//!
//! Consumers: the management surfaces (PMM-E `prx providers credential`,
//! PMM-F `PUT /api/providers/{id}/credential`) and the hosted decision
//! backends' `credential_ref` (CDP-13, D-040 OQ-A4, D-074).

use crate::auth::profiles::{AuthProfile, AuthProfilesStore};
use crate::config::Config;
use crate::config::credential_slots::validate_credential;
use crate::providers::ProviderId;
use crate::providers::credentials::CredentialRef;
use chrono::{DateTime, Utc};
use serde::Serialize;

/// Every credential reference id starts with this.
pub const REFERENCE_PREFIX: &str = "cred-";

/// The longest name after [`REFERENCE_PREFIX`].
const MAX_NAME_CHARS: usize = 48;

/// A credential value in transit to the store: zeroed when dropped, and
/// never printed (`Debug` shows `[REDACTED]`).
pub struct SecretString(zeroize::Zeroizing<String>);

impl SecretString {
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(zeroize::Zeroizing::new(value))
    }

    /// The value, for the one write that stores it.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

/// What a surface may show about a credential reference: never its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialStatus {
    /// `cred-<name>`.
    pub id: String,
    /// The provider it was issued for, URL userinfo redacted.
    pub owner: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Why a credential reference operation was refused. The text never holds a
/// credential value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialReferenceError {
    #[error(
        "a credential name is 1 to {MAX_NAME_CHARS} characters of lowercase ASCII letters, digits and `-`, \
         starting with a letter or digit"
    )]
    InvalidName,
    /// The owner text is never echoed: a secret pasted into the owner
    /// position must not come back (D-078 T-6).
    #[error("the owner is not a provider PRX can construct; use a name from `prx providers` or `custom:<url>`")]
    UnknownOwner,
    #[error(
        "the owner's URL carries `user:password@`; register the credential for the endpoint without it (the \
         URL password would be stored unencrypted as the owner and would orphan the reference when it changes)"
    )]
    OwnerCarriesUserinfo,
    #[error("the credential value is refused: {0}")]
    InvalidValue(&'static str),
    #[error("credential reference '{0}' does not exist")]
    Missing(String),
    /// Names the reference and the provider it belongs to (as stored at
    /// `set`, userinfo-free); never the caller's target text.
    #[error("credential reference '{id}' belongs to provider '{owner}', not to the provider this selection targets")]
    OwnedByOther { id: String, owner: String },
    #[error("'{0}' is not a credential reference id (they start with `cred-`)")]
    NotAReference(String),
    #[error("the credential store could not be read or written: {0}")]
    Store(String),
}

/// The credential references of one configuration directory.
pub struct CredentialReferences {
    store: AuthProfilesStore,
}

impl std::fmt::Debug for CredentialReferences {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialReferences")
            .field("store", &self.store.path())
            .finish()
    }
}

impl CredentialReferences {
    /// The references stored next to `config`'s `config.toml`, encrypted at
    /// rest when `config.secrets.encrypt` is on.
    ///
    /// # Errors
    ///
    /// When the configuration has no directory.
    pub fn for_config(config: &Config) -> Result<Self, CredentialReferenceError> {
        let dir = config
            .config_path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .ok_or_else(|| CredentialReferenceError::Store("the configuration has no directory".to_string()))?;
        Ok(Self {
            store: AuthProfilesStore::new(dir, config.secrets.encrypt),
        })
    }

    /// The id a credential named `name` is stored under.
    ///
    /// # Errors
    ///
    /// When `name` is not a valid credential name.
    pub fn id_for(name: &str) -> Result<String, CredentialReferenceError> {
        let valid = !name.is_empty()
            && name.chars().count() <= MAX_NAME_CHARS
            && name
                .bytes()
                .next()
                .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if valid {
            Ok(format!("{REFERENCE_PREFIX}{name}"))
        } else {
            Err(CredentialReferenceError::InvalidName)
        }
    }

    /// The selection reference for `id`.
    ///
    /// # Errors
    ///
    /// When `id` is not a credential reference id.
    pub fn reference(id: &str) -> Result<CredentialRef, CredentialReferenceError> {
        if id
            .strip_prefix(REFERENCE_PREFIX)
            .is_some_and(|name| Self::id_for(name).is_ok())
        {
            Ok(CredentialRef::Profile { id: id.to_string() })
        } else {
            Err(CredentialReferenceError::NotAReference(id.escape_default().to_string()))
        }
    }

    /// Store `secret` as the credential `name` of provider `owner`, creating
    /// or replacing it. A reference is never moved to another provider: an
    /// existing `name` owned by a different provider is refused.
    ///
    /// # Errors
    ///
    /// An invalid name, owner or value, a name owned by another provider, or
    /// a store failure. No error carries the value.
    pub fn set(&self, owner: &str, name: &str, secret: &str) -> Result<CredentialStatus, CredentialReferenceError> {
        let id = Self::id_for(name)?;
        let owner = owner.trim();
        if matches!(
            crate::providers::redact_endpoint_userinfo(owner),
            std::borrow::Cow::Owned(_)
        ) {
            return Err(CredentialReferenceError::OwnerCarriesUserinfo);
        }
        let owner_id = ProviderId::of(owner);
        if !owner_id.is_resolvable() {
            return Err(CredentialReferenceError::UnknownOwner);
        }
        let secret = validate_credential(secret).map_err(|error| match error {
            crate::config::credential_slots::SlotError::InvalidValue(reason) => {
                CredentialReferenceError::InvalidValue(reason)
            }
            _ => CredentialReferenceError::InvalidValue("not a credential"),
        })?;
        let mut profile = AuthProfile::new_token(owner, name, secret);
        profile.id.clone_from(&id);
        // The owner check runs under the store's lock, with the write: a
        // concurrent `set` can never move the reference to another provider.
        let mut refusal = None;
        self.store
            .upsert_profile_checked(profile, |existing| {
                let Some(existing) = existing else {
                    return true;
                };
                let existing_owner = ProviderId::of(&existing.provider);
                if existing_owner == owner_id {
                    return true;
                }
                refusal = Some(CredentialReferenceError::OwnedByOther {
                    id: id.clone(),
                    owner: existing_owner.to_string(),
                });
                false
            })
            .map_err(|error| store_error(&error))?;
        if let Some(refusal) = refusal {
            return Err(refusal);
        }
        self.status(&id)?.ok_or(CredentialReferenceError::Missing(id))
    }

    /// Remove the credential `id`. `Ok(false)` when it did not exist.
    ///
    /// # Errors
    ///
    /// When `id` is not a credential reference id, or on a store failure.
    pub fn remove(&self, id: &str) -> Result<bool, CredentialReferenceError> {
        Self::reference(id)?;
        self.store.remove_profile(id).map_err(|error| store_error(&error))
    }

    /// The status of `id`, or `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// When `id` is not a credential reference id, or on a store failure.
    pub fn status(&self, id: &str) -> Result<Option<CredentialStatus>, CredentialReferenceError> {
        Self::reference(id)?;
        Ok(self.load()?.profiles.get(id).map(status_of))
    }

    /// Every credential reference, by id.
    ///
    /// # Errors
    ///
    /// On a store failure.
    pub fn list(&self) -> Result<Vec<CredentialStatus>, CredentialReferenceError> {
        Ok(self
            .load()?
            .profiles
            .values()
            .filter(|profile| profile.id.starts_with(REFERENCE_PREFIX))
            .map(status_of)
            .collect())
    }

    /// Check that `id` exists and was issued for the provider `target`
    /// names. This is the rule a configuration that stores a reference is
    /// validated with, and the one the resolver applies before it releases
    /// the material.
    ///
    /// # Errors
    ///
    /// A missing reference, one owned by another provider, or a store
    /// failure.
    pub fn check_reference(&self, id: &str, target: &str) -> Result<CredentialStatus, CredentialReferenceError> {
        Self::reference(id)?;
        let data = self.load()?;
        let profile = data
            .profiles
            .get(id)
            .ok_or_else(|| CredentialReferenceError::Missing(id.to_string()))?;
        let owner = ProviderId::of(&profile.provider);
        if owner != ProviderId::of(target) {
            return Err(CredentialReferenceError::OwnedByOther {
                id: id.to_string(),
                owner: owner.to_string(),
            });
        }
        Ok(status_of(profile))
    }

    fn load(&self) -> Result<crate::auth::profiles::AuthProfilesData, CredentialReferenceError> {
        self.store.load_read_only().map_err(|error| store_error(&error))
    }
}

fn status_of(profile: &AuthProfile) -> CredentialStatus {
    CredentialStatus {
        id: profile.id.clone(),
        owner: ProviderId::of(&profile.provider).to_string(),
        created_at: profile.created_at,
        updated_at: profile.updated_at,
    }
}

/// A store error, sanitized: the store names paths and parse positions, and
/// an upstream-style message could echo a value.
fn store_error(error: &anyhow::Error) -> CredentialReferenceError {
    CredentialReferenceError::Store(crate::providers::sanitize_api_error(&format!("{error:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::catalog::{self, CatalogView, Requirement, Selection, UnknownPolicy};
    use crate::providers::credentials::ConfigCredentialSource;

    /// A credential-shaped sentinel. Not a real secret.
    const SECRET: &str = "pmmd6-reference-sentinel";

    fn config_in(dir: &std::path::Path, encrypt: bool) -> Config {
        let mut config = Config {
            config_path: dir.join("config.toml"),
            workspace_dir: dir.join("workspace"),
            ..Config::default()
        };
        config.secrets.encrypt = encrypt;
        config
    }

    fn references(dir: &std::path::Path) -> (Config, CredentialReferences) {
        let config = config_in(dir, true);
        let references = CredentialReferences::for_config(&config).expect("test: store");
        (config, references)
    }

    /// PMM-D-6: `set` stores the value encrypted at rest; no answer — the
    /// status, the listing, the refusals, `Debug` — carries it.
    #[test]
    fn a_reference_is_stored_encrypted_and_never_echoed() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let (_config, references) = references(dir.path());

        let status = references.set("openai", "route-key", SECRET).expect("test: set");

        assert_eq!(status.id, "cred-route-key");
        let stored = std::fs::read_to_string(references.store.path()).expect("test: store file");
        assert!(!stored.contains(SECRET), "stored in plaintext: {stored}");
        assert!(stored.contains("enc2:"), "not encrypted: {stored}");
        let shown = format!(
            "{status:?} {:?} {:?} {references:?}",
            references.list().expect("test: list"),
            references.check_reference("cred-route-key", "anthropic"),
        );
        assert!(!shown.contains(SECRET), "{shown}");
    }

    #[test]
    fn a_reference_is_never_moved_to_another_provider() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let (_config, references) = references(dir.path());
        references.set("openai", "shared", SECRET).expect("test: set");

        let refusal = references
            .set("anthropic", "shared", "pmmd6-other-sentinel")
            .expect_err("test: moving a reference is refused");

        assert!(
            matches!(refusal, CredentialReferenceError::OwnedByOther { .. }),
            "{refusal}"
        );
        assert!(!refusal.to_string().contains("sentinel"), "{refusal}");
        assert_eq!(
            references
                .check_reference("cred-shared", "openai")
                .map(|status| status.id),
            Ok("cred-shared".to_string())
        );
        // Replacing the value for the same provider is allowed.
        references
            .set("OpenAI", "shared", "pmmd6-rotated-sentinel")
            .expect("test: rotate");
    }

    #[test]
    fn a_reference_is_checked_against_the_provider_it_targets() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let (_config, references) = references(dir.path());
        references
            .set("custom:http://127.0.0.1:9000/v1", "local", SECRET)
            .expect("test: set");

        assert!(
            references
                .check_reference("cred-local", "custom:http://127.0.0.1:9000/v1/")
                .is_ok()
        );
        for target in ["custom:http://127.0.0.1:9001/v1", "openai"] {
            assert!(
                matches!(
                    references.check_reference("cred-local", target),
                    Err(CredentialReferenceError::OwnedByOther { .. })
                ),
                "{target}"
            );
        }
        assert_eq!(
            references.check_reference("cred-absent", "openai"),
            Err(CredentialReferenceError::Missing("cred-absent".into()))
        );
    }

    #[test]
    fn names_owners_and_values_are_validated() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let (_config, references) = references(dir.path());
        for name in ["", "Upper", "-lead", "has space", "ſ", "a:b", &"x".repeat(49)] {
            assert_eq!(
                references.set("openai", name, SECRET),
                Err(CredentialReferenceError::InvalidName),
                "{name:?}"
            );
        }
        assert_eq!(
            references.set("not-a-provider", "k", SECRET),
            Err(CredentialReferenceError::UnknownOwner)
        );
        for value in ["", "***", "enc2:00", "line\nbreak"] {
            assert!(
                matches!(
                    references.set("openai", "k", value),
                    Err(CredentialReferenceError::InvalidValue(_))
                ),
                "{value:?}"
            );
        }
        assert!(matches!(
            references.remove("openai:default"),
            Err(CredentialReferenceError::NotAReference(_))
        ));
        assert!(references.list().expect("test: list").is_empty());
    }

    #[test]
    fn remove_and_list_see_references_only() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let (_config, references) = references(dir.path());
        references
            .store
            .upsert_profile(
                AuthProfile::new_token("openai", "default", "pmmd6-own-profile".into()),
                true,
            )
            .expect("test: the provider's own profile");
        references.set("openai", "a", SECRET).expect("test: set");

        let listed = references.list().expect("test: list");
        assert_eq!(
            listed.iter().map(|status| status.id.as_str()).collect::<Vec<_>>(),
            vec!["cred-a"]
        );
        assert_eq!(references.remove("cred-a"), Ok(true));
        assert_eq!(references.remove("cred-a"), Ok(false));
        assert!(references.list().expect("test: list").is_empty());
    }

    /// The resolver releases a reference's material only to the provider it
    /// was issued for.
    #[test]
    fn the_resolver_releases_a_reference_to_its_owner_only() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let (config, references) = references(dir.path());
        references.set("openai", "route-key", SECRET).expect("test: set");
        let reference = CredentialReferences::reference("cred-route-key").expect("test: reference");
        let catalog = CatalogView::from_config(&config);

        let resolved = catalog::resolve(
            &Selection {
                provider: "openai",
                model: "gpt-4o",
                credential: reference.clone(),
            },
            &Requirement::chat(),
            &catalog,
            &ConfigCredentialSource::new(&config),
            UnknownPolicy::Invocation,
        )
        .expect("test: released to its owner");
        assert_eq!(resolved.credential().api_key(), Some(SECRET));

        let refusal = catalog::resolve(
            &Selection {
                provider: "anthropic",
                model: "claude-sonnet-4",
                credential: reference,
            },
            &Requirement::chat(),
            &catalog,
            &ConfigCredentialSource::new(&config),
            UnknownPolicy::Invocation,
        )
        .expect_err("test: never released to another provider");
        assert!(!refusal.to_string().contains(SECRET), "{refusal}");
    }

    /// RED before PMMDtail-f (D-078 T-6): an owner URL with userinfo was
    /// stored as the (unencrypted) owner, and an unknown owner was echoed.
    #[test]
    fn the_owner_is_stored_without_userinfo_and_never_echoed() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let (_config, references) = references(dir.path());

        let refusal = references
            .set("custom:http://alice:pmmd6-url-password@127.0.0.1:9/v1", "k", SECRET)
            .expect_err("test: an owner with userinfo is refused");
        assert!(!refusal.to_string().contains("pmmd6-url-password"), "{refusal}");
        assert!(
            !std::fs::read_to_string(references.store.path())
                .unwrap_or_default()
                .contains("pmmd6-url-password"),
            "the URL password reached the store"
        );

        let pasted = "sk-pmmd6-pasted-into-the-owner";
        let refusal = references.set(pasted, "k", SECRET).expect_err("test: unknown owner");
        assert!(!refusal.to_string().contains(pasted), "{refusal}");
        assert!(!format!("{refusal:?}").contains(pasted), "{refusal:?}");
    }
}
