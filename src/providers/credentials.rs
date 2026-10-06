//! Credential references and their owner-bound resolution.
//!
//! A selection names its credential by *reference*, never by value. A
//! reference resolves to material only when the provider it was issued for —
//! its owner — is the provider identity the selection targets. That rule lives
//! in [`crate::providers::catalog::resolve`], which asks a [`CredentialSource`]
//! for the owner first and for the material only after the owner matched; a
//! source never decides whether to release.
//!
//! The root `api_key`, `api_url` and `reliability.api_keys` are one
//! credential: they belong to the configured default provider and travel
//! together or not at all (R-C0f).

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::fmt;

use crate::auth::profiles::{AuthProfilesData, AuthProfilesStore};
use crate::config::credential_slots::CredentialSlot;
use crate::config::{Config, DelegateAgentConfig, EmbeddingRouteConfig, ModelRouteConfig};
use crate::providers::{DEFAULT_PROVIDER_FOR_UNSET, ProviderId};

/// A legacy inline credential slot in configuration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LegacySlot {
    /// `api_key`, with `api_url` and `reliability.api_keys`; owned by the
    /// configured default provider.
    Root,
    /// `agents.<name>.api_key`; owned by `agents.<name>.provider`.
    Agent(String),
    /// `model_routes[hint].api_key`; owned by that route's provider.
    ModelRoute(String),
    /// `embedding_routes[hint].api_key`; owned by that route's provider.
    EmbeddingRoute(String),
}

impl LegacySlot {
    fn slot(&self) -> CredentialSlot {
        match self {
            Self::Root => CredentialSlot::Root,
            Self::Agent(name) => CredentialSlot::Agent(name.clone()),
            Self::ModelRoute(hint) => CredentialSlot::ModelRoute(hint.clone()),
            Self::EmbeddingRoute(hint) => CredentialSlot::EmbeddingRoute(hint.clone()),
        }
    }
}

impl fmt::Display for LegacySlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.slot().fmt(f)
    }
}

/// Which credential a selection may use.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CredentialRef {
    /// Deliberately anonymous. Refused for a provider whose adapter needs a
    /// credential.
    None,
    /// Nothing configured is released; the provider's own store lookup
    /// (its auth profile or OAuth cache) applies at construction, exactly as
    /// it does today.
    ProviderStore,
    /// One auth-profile entry, by id. Owned by the profile's `provider`.
    Profile { id: String },
    /// A legacy inline slot in configuration.
    LegacyInline(LegacySlot),
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("no credential"),
            Self::ProviderStore => f.write_str("the provider's own credential store"),
            Self::Profile { id } => write!(f, "auth profile '{id}'"),
            Self::LegacyInline(slot) => write!(f, "`{slot}`"),
        }
    }
}

/// The credential material behind one reference, released as a unit.
///
/// No `Serialize`, no `Display`, and `Debug` shows only which parts are
/// present.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialMaterial {
    api_key: Option<String>,
    api_url: Option<String>,
    rotation_keys: Vec<String>,
}

impl CredentialMaterial {
    /// Material with only a secret.
    #[must_use]
    pub fn secret(api_key: &str) -> Self {
        Self {
            api_key: non_empty(Some(api_key)),
            api_url: None,
            rotation_keys: Vec::new(),
        }
    }

    /// The root connection: key, base URL and rotation keys together.
    #[must_use]
    pub fn connection(api_key: Option<&str>, api_url: Option<&str>, rotation_keys: &[String]) -> Self {
        Self {
            api_key: non_empty(api_key),
            api_url: non_empty(api_url),
            rotation_keys: rotation_keys
                .iter()
                .map(|key| key.trim())
                .filter(|key| !key.is_empty())
                .map(ToString::to_string)
                .collect(),
        }
    }

    /// The secret, if any.
    #[must_use]
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// The base URL that travels with the secret, if any.
    #[must_use]
    pub fn api_url(&self) -> Option<&str> {
        self.api_url.as_deref()
    }

    /// The rotation keys that travel with the secret.
    #[must_use]
    pub fn rotation_keys(&self) -> &[String] {
        &self.rotation_keys
    }

    /// Whether nothing at all is present.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.api_key.is_none() && self.api_url.is_none() && self.rotation_keys.is_empty()
    }
}

impl fmt::Debug for CredentialMaterial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialMaterial")
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("api_url", &self.api_url.as_deref().map(super::redact_endpoint_userinfo))
            .field("rotation_keys", &self.rotation_keys.len())
            .finish()
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// Why a credential source could not answer. The text never holds a value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialLookupError {
    /// More than one entry carries the name the reference uses.
    #[error("more than one entry matches {0}")]
    Ambiguous(String),
    /// The backing store could not be read.
    #[error("the credential store could not be read: {0}")]
    Store(String),
}

/// Where credential references are looked up.
///
/// A source reports facts only. The release decision — owner equals target —
/// is the resolver's, so no implementation can hand one provider's secret to
/// another by answering differently.
pub trait CredentialSource {
    /// The provider `reference` was issued for, or `None` when it names
    /// nothing that exists.
    ///
    /// # Errors
    ///
    /// When the backing store cannot be read or the reference is ambiguous.
    fn owner_of(&self, reference: &CredentialRef) -> Result<Option<ProviderId>, CredentialLookupError>;

    /// The material behind `reference`. Called only after its owner matched.
    ///
    /// # Errors
    ///
    /// When the backing store cannot be read or the reference is ambiguous.
    fn material(&self, reference: &CredentialRef) -> Result<Option<CredentialMaterial>, CredentialLookupError>;
}

/// Credential references resolved against a loaded [`Config`] and the
/// encrypted auth-profile store next to it.
pub struct ConfigCredentialSource<'a> {
    config: &'a Config,
    profiles: OnceCell<Result<AuthProfilesData, String>>,
}

impl<'a> ConfigCredentialSource<'a> {
    /// A source over `config`. The auth-profile store is read on first use.
    #[must_use]
    pub const fn new(config: &'a Config) -> Self {
        Self {
            config,
            profiles: OnceCell::new(),
        }
    }

    fn profiles(&self) -> Result<&AuthProfilesData, CredentialLookupError> {
        self.profiles
            .get_or_init(|| {
                let Some(dir) = self.config.config_path.parent() else {
                    return Ok(AuthProfilesData::default());
                };
                AuthProfilesStore::new(dir, self.config.secrets.encrypt)
                    .load_read_only()
                    .map_err(|error| super::sanitize_api_error(&error.to_string()))
            })
            .as_ref()
            .map_err(|reason| CredentialLookupError::Store(reason.clone()))
    }

    fn unique<'c, T>(
        entries: impl Iterator<Item = &'c T>,
        reference: &CredentialRef,
    ) -> Result<Option<&'c T>, CredentialLookupError> {
        let mut entries = entries;
        let first = entries.next();
        if entries.next().is_some() {
            return Err(CredentialLookupError::Ambiguous(reference.to_string()));
        }
        Ok(first)
    }

    /// The owner spelling and the material of a legacy slot.
    fn slot(
        &self,
        slot: &LegacySlot,
        reference: &CredentialRef,
    ) -> Result<Option<(&'a str, CredentialMaterial)>, CredentialLookupError> {
        let config = self.config;
        Ok(match slot {
            LegacySlot::Root => Some((
                config.default_provider.as_deref().unwrap_or(DEFAULT_PROVIDER_FOR_UNSET),
                CredentialMaterial::connection(
                    config.api_key.as_deref(),
                    config.api_url.as_deref(),
                    &config.reliability.api_keys,
                ),
            )),
            LegacySlot::Agent(name) => config.agents.get(name).map(|agent| {
                (
                    agent.provider.as_str(),
                    CredentialMaterial::connection(agent.api_key.as_deref(), None, &[]),
                )
            }),
            LegacySlot::ModelRoute(hint) => Self::unique(
                config.model_routes.iter().filter(|route| route.hint == *hint),
                reference,
            )?
            .map(|route| {
                (
                    route.provider.as_str(),
                    CredentialMaterial::connection(route.api_key.as_deref(), None, &[]),
                )
            }),
            LegacySlot::EmbeddingRoute(hint) => Self::unique(
                config.embedding_routes.iter().filter(|route| route.hint == *hint),
                reference,
            )?
            .map(|route| {
                (
                    route.provider.as_str(),
                    CredentialMaterial::connection(route.api_key.as_deref(), None, &[]),
                )
            }),
        })
    }
}

impl CredentialSource for ConfigCredentialSource<'_> {
    fn owner_of(&self, reference: &CredentialRef) -> Result<Option<ProviderId>, CredentialLookupError> {
        match reference {
            CredentialRef::None | CredentialRef::ProviderStore => Ok(None),
            CredentialRef::Profile { id } => Ok(self
                .profiles()?
                .profiles
                .get(id)
                .map(|profile| ProviderId::of(&profile.provider))),
            CredentialRef::LegacyInline(slot) => {
                Ok(self.slot(slot, reference)?.map(|(owner, _)| ProviderId::of(owner)))
            }
        }
    }

    fn material(&self, reference: &CredentialRef) -> Result<Option<CredentialMaterial>, CredentialLookupError> {
        match reference {
            CredentialRef::None | CredentialRef::ProviderStore => Ok(None),
            CredentialRef::Profile { id } => Ok(self.profiles()?.profiles.get(id).and_then(|profile| {
                profile
                    .token
                    .as_deref()
                    .or_else(|| profile.token_set.as_ref().map(|tokens| tokens.access_token.as_str()))
                    .map(CredentialMaterial::secret)
            })),
            CredentialRef::LegacyInline(slot) => Ok(self.slot(slot, reference)?.map(|(_, material)| material)),
        }
    }
}

/// The key a route, an embedding route or an agent sends to `provider`: its
/// inline `api_key`, or else the material behind its `credential_ref`
/// (PMM-H, SP-PMM §2.4). A reference is released only when it was issued for
/// `provider` — the resolver's owner rule, applied through the same
/// [`CredentialSource`] — so no reference can hand one provider's credential
/// to another. A reference that is missing, owned by another provider or
/// unreadable yields no key (the provider's own store lookup applies, as
/// without one) and a warning naming the reference id only.
#[must_use]
pub fn configured_key(
    source: &dyn CredentialSource,
    provider: &str,
    api_key: Option<&str>,
    credential_ref: Option<&str>,
) -> Option<String> {
    if let Some(key) = api_key.map(str::trim).filter(|key| !key.is_empty()) {
        return Some(key.to_string());
    }
    let id = credential_ref.map(str::trim).filter(|id| !id.is_empty())?;
    let reference = CredentialRef::Profile { id: id.to_string() };
    match source.owner_of(&reference) {
        Ok(Some(owner)) if owner == ProviderId::of(provider) => {}
        Ok(Some(_)) => {
            tracing::warn!(
                reference = id,
                "credential reference was issued for another provider; it is not sent"
            );
            return None;
        }
        Ok(None) => {
            tracing::warn!(reference = id, "credential reference does not exist; it is not sent");
            return None;
        }
        Err(error) => {
            tracing::warn!(reference = id, error = %error, "credential reference could not be read");
            return None;
        }
    }
    match source.material(&reference) {
        Ok(material) => material.and_then(|material| material.api_key().map(ToString::to_string)),
        Err(error) => {
            tracing::warn!(reference = id, error = %error, "credential reference could not be read");
            None
        }
    }
}

impl Config {
    /// The model routes as the provider factory takes them: a route
    /// with a `credential_ref` carries the referenced key in its `api_key`
    /// (in memory only; this copy is never saved). Borrowed when no route names
    /// a reference.
    #[must_use]
    pub fn model_routes_with_references(&self) -> Cow<'_, [ModelRouteConfig]> {
        if self.model_routes.iter().all(|route| route.credential_ref.is_none()) {
            return Cow::Borrowed(&self.model_routes);
        }
        let source = ConfigCredentialSource::new(self);
        Cow::Owned(
            self.model_routes
                .iter()
                .map(|route| ModelRouteConfig {
                    api_key: configured_key(
                        &source,
                        &route.provider,
                        route.api_key.as_deref(),
                        route.credential_ref.as_deref(),
                    ),
                    ..route.clone()
                })
                .collect(),
        )
    }

    /// The embedding routes with each `credential_ref` resolved into
    /// the in-memory `api_key`, as [`Config::model_routes_with_references`] does.
    #[must_use]
    pub fn embedding_routes_with_references(&self) -> Cow<'_, [EmbeddingRouteConfig]> {
        if self.embedding_routes.iter().all(|route| route.credential_ref.is_none()) {
            return Cow::Borrowed(&self.embedding_routes);
        }
        let source = ConfigCredentialSource::new(self);
        Cow::Owned(
            self.embedding_routes
                .iter()
                .map(|route| EmbeddingRouteConfig {
                    api_key: configured_key(
                        &source,
                        &route.provider,
                        route.api_key.as_deref(),
                        route.credential_ref.as_deref(),
                    ),
                    ..route.clone()
                })
                .collect(),
        )
    }

    /// The named agents with each `credential_ref` resolved into the
    /// in-memory `api_key`, as [`Config::model_routes_with_references`] does.
    #[must_use]
    pub fn agents_with_references(&self) -> Cow<'_, HashMap<String, DelegateAgentConfig>> {
        if self.agents.values().all(|agent| agent.credential_ref.is_none()) {
            return Cow::Borrowed(&self.agents);
        }
        let source = ConfigCredentialSource::new(self);
        Cow::Owned(
            self.agents
                .iter()
                .map(|(name, agent)| {
                    (
                        name.clone(),
                        DelegateAgentConfig {
                            api_key: configured_key(
                                &source,
                                &agent.provider,
                                agent.api_key.as_deref(),
                                agent.credential_ref.as_deref(),
                            ),
                            ..agent.clone()
                        },
                    )
                })
                .collect(),
        )
    }
}
