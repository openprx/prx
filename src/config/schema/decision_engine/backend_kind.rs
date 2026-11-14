//! The backend kind and locality settings and the one reading of the
//! configured backend selection, shared by configuration checks and the
//! decision engine factory (moved from `decision::{protocol, backend,
//! factory}`, R17).

use super::{BackendSelection, CredentialRefSetting, DecisionBackendConfig};
use crate::providers::credentials::CredentialRef;
use crate::providers::loopback::is_literal_loopback;

/// The kind of backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// The local deterministic backend.
    Builtin,
    /// Any System One compatible HTTP endpoint.
    CompatibleHttp,
    /// The hosted TypeSafe adapter.
    TypeSafe,
    /// The hosted OpenRouter Decisions adapter.
    OpenRouter,
    /// The scripted test backend.
    #[cfg(any(test, feature = "test-mock"))]
    Mock,
}

impl BackendKind {
    /// The stable kind name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::CompatibleHttp => "compatible_http",
            Self::TypeSafe => "typesafe",
            Self::OpenRouter => "openrouter",
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock => "mock",
        }
    }
}

/// Where a backend runs. It is derived from the backend kind and the literal
/// loopback predicate, never from a URL alone for hosted kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendLocality {
    /// In process.
    Builtin,
    /// On a literal loopback address.
    Loopback,
    /// Anywhere else.
    Remote,
}

impl BackendLocality {
    /// The stable locality name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Loopback => "loopback",
            Self::Remote => "remote",
        }
    }
}

/// Where a backend of `kind` configured with `endpoint` runs (SP-CDP-A 5.6),
/// derived from the kind and the literal-loopback predicate:
///
/// - `builtin` runs in process;
/// - `compatible_http` is on loopback when its endpoint is written as a
///   literal loopback address, and remote otherwise;
/// - a hosted kind is always remote, even with an endpoint override that
///   reads as loopback (COORD-6).
#[must_use]
pub fn locality_of(kind: BackendKind, endpoint: &str) -> BackendLocality {
    match kind {
        BackendKind::Builtin => BackendLocality::Builtin,
        BackendKind::CompatibleHttp if is_literal_loopback(endpoint) => BackendLocality::Loopback,
        BackendKind::CompatibleHttp | BackendKind::TypeSafe | BackendKind::OpenRouter => BackendLocality::Remote,
        #[cfg(any(test, feature = "test-mock"))]
        BackendKind::Mock => BackendLocality::Builtin,
    }
}

/// The configured non-builtin backend kind, if any: `auto` with an
/// endpoint is the compatible backend, `auto` without one and `builtin` are
/// none. The one reading of the `kind` setting, shared with the
/// configuration's risk rule (D-082 N-5).
#[must_use]
pub fn configured_kind(backend: &DecisionBackendConfig) -> Option<BackendKind> {
    match backend.kind {
        BackendSelection::Auto if backend.endpoint.trim().is_empty() => None,
        BackendSelection::Auto => Some(BackendKind::CompatibleHttp),
        BackendSelection::Fixed(BackendKind::Builtin) => None,
        BackendSelection::Fixed(kind) => Some(kind),
    }
}

/// The provider a configured backend belongs to.
fn provider_of(kind: BackendKind, endpoint: &str) -> String {
    match kind {
        BackendKind::CompatibleHttp => format!("custom:{endpoint}"),
        other => other.as_str().to_string(),
    }
}

/// The credential reference the strict selection checks (D-040 OQ-A4,
/// D-074): an explicit `"none"` is anonymous; an unset reference is
/// anonymous only on a literal loopback endpoint, and otherwise names the
/// provider's own store, which a hosted backend refuses (it takes an auth
/// profile only).
fn credential_ref_for(setting: &CredentialRefSetting, locality: BackendLocality) -> CredentialRef {
    match (setting, locality) {
        (CredentialRefSetting::None, _) | (CredentialRefSetting::Unset, BackendLocality::Loopback) => {
            CredentialRef::None
        }
        (CredentialRefSetting::Unset, BackendLocality::Builtin | BackendLocality::Remote) => {
            CredentialRef::ProviderStore
        }
        (CredentialRefSetting::Profile(id), _) => CredentialRef::Profile { id: id.clone() },
    }
}

/// The backend an effective engine backend configures (R55), as the strict
/// selection sees it; `None` for the builtin, or `auto` without an
/// endpoint. The factory builds from it and the configuration check
/// (`config::manage::validate`) checks it, so both read one rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredSelection {
    pub kind: BackendKind,
    pub locality: BackendLocality,
    /// The provider id the backend belongs to (the owner of its credential).
    pub provider: String,
    /// The credential the strict selection checks.
    pub credential: CredentialRef,
}

/// The configured selection of `backend` (the effective engine backend,
/// `decision_engine::config::effective_engine_backend`, for every engine use).
#[must_use]
pub fn configured_selection(backend: &DecisionBackendConfig) -> Option<ConfiguredSelection> {
    let kind = configured_kind(backend)?;
    let endpoint = backend.endpoint.trim();
    let locality = locality_of(kind, endpoint);
    Some(ConfiguredSelection {
        kind,
        locality,
        provider: provider_of(kind, endpoint),
        credential: credential_ref_for(&backend.credential_ref, locality),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locality_follows_the_kind_and_the_written_endpoint() {
        assert_eq!(locality_of(BackendKind::Builtin, ""), BackendLocality::Builtin);
        assert_eq!(
            locality_of(BackendKind::CompatibleHttp, "http://127.0.0.1:8900/v1"),
            BackendLocality::Loopback
        );
        assert_eq!(
            locality_of(BackendKind::CompatibleHttp, "http://127.1:8900/v1"),
            BackendLocality::Remote,
            "only a literal loopback spelling is loopback"
        );
        assert_eq!(
            locality_of(BackendKind::CompatibleHttp, "https://decisions.example.com/v1"),
            BackendLocality::Remote
        );
        for hosted in [BackendKind::TypeSafe, BackendKind::OpenRouter] {
            assert_eq!(
                locality_of(hosted, "http://127.0.0.1:8900/v1"),
                BackendLocality::Remote,
                "COORD-6: a hosted override that reads as loopback stays remote"
            );
        }
    }

    /// D-040 OQ-A4, D-074: anonymous only when asked for, or on a literal
    /// loopback endpoint; otherwise the provider's own store, which a hosted
    /// backend refuses.
    #[test]
    fn an_unset_credential_is_anonymous_only_on_loopback() {
        assert_eq!(
            credential_ref_for(&CredentialRefSetting::Unset, BackendLocality::Loopback),
            CredentialRef::None
        );
        assert_eq!(
            credential_ref_for(&CredentialRefSetting::Unset, BackendLocality::Remote),
            CredentialRef::ProviderStore
        );
        for locality in [BackendLocality::Loopback, BackendLocality::Remote] {
            assert_eq!(
                credential_ref_for(&CredentialRefSetting::None, locality),
                CredentialRef::None
            );
        }
    }
}
