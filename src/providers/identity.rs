//! Provider identity: what a configured provider string names, the one
//! comparable key for it, and the release rule for the root connection.
//!
//! Every reader that decides "is this the same provider" goes through
//! [`classify_provider`] and [`canonical_provider_key`] here: the factory,
//! model validation, the credential gate and the tool-dialect declarations.

use super::{canonical_china_provider_name, canonical_provider_info, parse_custom_provider_url, regional_endpoint_for};
use std::sync::Arc;

/// The provider an unset `default_provider` means.
///
/// Every reader that falls back for an unset default names this constant (or
/// [`ProviderId::default_for_unset`]) instead of repeating the literal, so the
/// fallback and the credential it releases cannot drift apart.
pub const DEFAULT_PROVIDER_FOR_UNSET: &str = "openrouter";

/// Which kind of thing a [`ProviderId`] names. It is also the key's prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderNamespace {
    /// A catalog row, possibly with the regional endpoint its alias selects.
    Catalog,
    /// An OpenAI-compatible endpoint the operator runs (`custom:<url>`).
    Custom,
    /// An Anthropic-compatible endpoint the operator runs (`anthropic-custom:<url>`).
    AnthropicCustom,
    /// A WASM plugin provider.
    Wasm,
    /// A decision-only service (`decision:<name>`, D-040 OQ-A3): it answers
    /// decision requests and nothing else, so no chat surface lists or
    /// builds it.
    DecisionService,
    /// Nothing this build can resolve.
    Unknown,
}

impl ProviderNamespace {
    const ALL: [Self; 6] = [
        Self::Catalog,
        Self::Custom,
        Self::AnthropicCustom,
        Self::Wasm,
        Self::DecisionService,
        Self::Unknown,
    ];

    const fn prefix(self) -> &'static str {
        match self {
            Self::Catalog => "catalog:",
            Self::Custom => "custom:",
            Self::AnthropicCustom => "anthropic-custom:",
            Self::Wasm => "wasm:",
            Self::DecisionService => "decision:",
            Self::Unknown => "unknown:",
        }
    }
}

/// Canonical, namespaced provider identity.
///
/// Every spelling of one provider maps to one id, and two different providers
/// never share one:
///
/// - a catalog row and its aliases: `catalog:openai`; an alias that selects a
///   regional endpoint keeps it, `catalog:qwen@https://dashscope-us…`, so a
///   credential issued for one region is not one for another;
/// - an endpoint the operator runs: its dialect and normalized URL,
///   `custom:http://127.0.0.1:8080/v1`, so a different port is a different
///   provider;
/// - a WASM plugin: `wasm:<name>`;
/// - anything else: `unknown:<lowercased spelling>`.
///
/// The namespace is what closes R-C1f G-2: an unresolvable spelling such as
/// `qwen@https://dashscope…` used to produce the very string a catalog key
/// is, and so shared that provider's released credential. It can now only
/// produce an `unknown:` id.
///
/// `Display` and `Debug` redact URL userinfo; [`as_str`](Self::as_str) is the
/// raw key, for comparison and storage only.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderId(Arc<str>);

impl ProviderId {
    /// The identity a configured provider string names.
    #[must_use]
    pub fn of(name: &str) -> Self {
        if let Some(service) = decision_service_name(name) {
            return Self(Arc::from(format!(
                "{}{service}",
                ProviderNamespace::DecisionService.prefix()
            )));
        }
        let key = match classify_provider(name) {
            ProviderIdentity::Catalog(canonical) => {
                let lowered = name.trim().to_ascii_lowercase();
                regional_endpoint_for(&lowered).map_or_else(
                    || format!("{}{canonical}", ProviderNamespace::Catalog.prefix()),
                    |endpoint| format!("{}{canonical}@{endpoint}", ProviderNamespace::Catalog.prefix()),
                )
            }
            ProviderIdentity::ByoEndpoint { dialect, base_url } => {
                format!("{}{}", dialect.prefix(), normalized_endpoint_url(&base_url))
            }
            // The spelling already starts with `wasm:`, which is the namespace.
            // The plugin index looks a provider up by its exported name,
            // case-sensitively, so the case is part of the identity.
            #[cfg(feature = "wasm-plugins")]
            ProviderIdentity::WasmPlugin => name.trim().to_string(),
            ProviderIdentity::Unknown => format!(
                "{}{}",
                ProviderNamespace::Unknown.prefix(),
                name.trim().to_ascii_lowercase()
            ),
        };
        Self(Arc::from(key))
    }

    /// Decode an identity from an authenticated process catalog, not a config label.
    /// The caller must authenticate the complete snapshot before calling this.
    /// Regional endpoints are pinned evidence from that snapshot; this does not
    /// resolve a provider or release a credential for an operator-supplied label.
    pub(super) fn from_authenticated_catalog_key(raw: &str) -> Option<Self> {
        if let Some(catalog) = raw.strip_prefix(ProviderNamespace::Catalog.prefix()) {
            let (name, endpoint) = catalog
                .split_once('@')
                .map_or((catalog, None), |(name, endpoint)| (name, Some(endpoint)));
            let configured = Self::of(name);
            if configured.namespace() != ProviderNamespace::Catalog || configured.catalog_name() != Some(name) {
                return None;
            }
            if let Some(endpoint) = endpoint {
                let url = reqwest::Url::parse(endpoint).ok()?;
                if !matches!(url.scheme(), "http" | "https")
                    || url.host_str().is_none()
                    || url.as_str().trim_end_matches('/') != endpoint
                {
                    return None;
                }
            } else if configured.as_str() != raw {
                return None;
            }
            return Some(Self(Arc::from(raw)));
        }
        if let Some(spelling) = raw.strip_prefix(ProviderNamespace::Unknown.prefix()) {
            let configured = Self::of(spelling);
            return (configured.as_str() == raw).then_some(configured);
        }
        let configured = Self::of(raw);
        (configured.as_str() == raw).then_some(configured)
    }

    /// The identity an unset `default_provider` means.
    #[must_use]
    pub fn default_for_unset() -> Self {
        Self::of(DEFAULT_PROVIDER_FOR_UNSET)
    }

    /// The raw key. It may carry URL userinfo: compare and store it, never
    /// print it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Which kind of thing this id names.
    #[must_use]
    pub fn namespace(&self) -> ProviderNamespace {
        // No prefix here is a prefix of another, so the order is irrelevant.
        ProviderNamespace::ALL
            .into_iter()
            .find(|namespace| self.0.starts_with(namespace.prefix()))
            .unwrap_or(ProviderNamespace::Unknown)
    }

    /// The canonical catalog row, without any regional endpoint, when this id
    /// names a catalog provider.
    #[must_use]
    pub fn catalog_name(&self) -> Option<&str> {
        let rest = self.0.strip_prefix(ProviderNamespace::Catalog.prefix())?;
        Some(rest.split_once('@').map_or(rest, |(name, _)| name))
    }

    /// Whether this identity names a runtime whose transport remains on this
    /// host. Local catalog adapters carry that fact in the compiled provider
    /// table; a bring-your-own endpoint is local only when its normalized URL
    /// has a loopback host.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_local_runtime(&self) -> bool {
        if let Some(name) = self.catalog_name() {
            return canonical_provider_info(name).is_some_and(|info| info.local);
        }
        let Some(endpoint) = self.0.strip_prefix(ProviderNamespace::Custom.prefix()) else {
            return false;
        };
        endpoint_is_loopback(endpoint)
    }

    /// Apply a root connection override after the caller has verified that
    /// this identity owns that configured endpoint.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_local_runtime_at(&self, root_endpoint: Option<&str>) -> bool {
        self.is_local_runtime() || root_endpoint.is_some_and(endpoint_is_loopback)
    }

    /// Whether this build can construct the provider this id names.
    #[must_use]
    pub fn is_resolvable(&self) -> bool {
        self.namespace() != ProviderNamespace::Unknown
    }
}

#[cfg(test)]
fn endpoint_is_loopback(endpoint: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&super::redact_endpoint_userinfo(&self.0))
    }
}

impl std::fmt::Debug for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("ProviderId")
            .field(&super::redact_endpoint_userinfo(&self.0))
            .finish()
    }
}

/// The decision-only services this build knows (D-040 OQ-A3). None of them is
/// a chat provider: they are not in `list_providers()`, and the chat factory
/// classifies them as unknown.
pub(crate) const DECISION_SERVICES: [&str; 1] = ["typesafe"];

/// The decision service a provider spelling names: `typesafe` or
/// `decision:typesafe`, in any ASCII case and with surrounding space.
fn decision_service_name(name: &str) -> Option<&'static str> {
    let lowered = name.trim().to_ascii_lowercase();
    let bare = lowered
        .strip_prefix(ProviderNamespace::DecisionService.prefix())
        .unwrap_or(&lowered);
    DECISION_SERVICES.into_iter().find(|service| *service == bare)
}

/// Where a provider's credential is stored in the auth-profile store.
///
/// This is the same fold the classifier applies, by calling it: a name that
/// resolved to two different providers here and there would withhold a key the
/// store itself says is shared. A bring-your-own endpoint keeps its literal
/// spelling, so a profile for one is one an operator created deliberately.
pub(crate) fn canonical_auth_profile_provider(name: &str) -> String {
    // A decision service's credential is owned by its namespaced id.
    if decision_service_name(name).is_some() {
        return ProviderId::of(name).as_str().to_string();
    }
    match classify_provider(name) {
        ProviderIdentity::Catalog(canonical) => canonical.to_string(),
        #[cfg(feature = "wasm-plugins")]
        ProviderIdentity::WasmPlugin => name.trim().to_ascii_lowercase(),
        ProviderIdentity::ByoEndpoint { .. } | ProviderIdentity::Unknown => name.trim().to_ascii_lowercase(),
    }
}

/// Which bring-your-own-endpoint dialect a provider string names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ByoDialect {
    /// `custom:<url>` — an OpenAI-compatible endpoint.
    OpenAiCompatible,
    /// `anthropic-custom:<url>` — an Anthropic-compatible endpoint.
    AnthropicCompatible,
}

impl ByoDialect {
    /// Every dialect the factory implements. No prefix here is a prefix of
    /// another, so the order is irrelevant.
    const ALL: [Self; 2] = [Self::OpenAiCompatible, Self::AnthropicCompatible];

    pub(super) const fn prefix(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "custom:",
            Self::AnthropicCompatible => "anthropic-custom:",
        }
    }

    /// The label the URL diagnostics already use for this dialect.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "Custom provider",
            Self::AnthropicCompatible => "Anthropic-custom provider",
        }
    }

    pub(super) const fn format_hint(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "custom:https://your-api.com",
            Self::AnthropicCompatible => "anthropic-custom:https://your-api.com",
        }
    }
}

/// What kind of thing a configured provider string names.
///
/// One classifier, three readers: the factory, model validation and the
/// credential gate. They previously each carried their own `starts_with`
/// knowledge (or none at all), which is why a bring-your-own endpoint could be
/// constructible and simultaneously unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderIdentity {
    /// A catalog provider, canonicalized through `list_providers()`.
    Catalog(&'static str),
    /// An endpoint the operator runs, named by dialect and base URL.
    ByoEndpoint { dialect: ByoDialect, base_url: String },
    /// A WASM plugin provider (`wasm:<name>`).
    #[cfg(feature = "wasm-plugins")]
    WasmPlugin,
    /// Nothing this build can resolve — including a bring-your-own prefix
    /// whose URL does not parse, which must never inherit BYO permissions.
    Unknown,
}

/// The bring-your-own dialect a provider label names, judged by its prefix.
///
/// For readers that need only the dialect and not a parsed endpoint — the
/// label-keyed tool-schema dialect map in `crate::tools::schema` — so that they
/// recognize exactly the prefixes, spellings and whitespace the factory does
/// instead of carrying their own `starts_with`. The URL is not validated here;
/// an unparseable one still fails at construction with its own diagnostic.
pub(crate) fn byo_dialect_of(name: &str) -> Option<ByoDialect> {
    split_byo_provider(name).map(|(dialect, _)| dialect)
}

/// Split a bring-your-own provider string into its dialect and its raw URL.
///
/// The prefix comparison is ASCII-case-insensitive, like every other provider
/// name comparison in this module.
pub(super) fn split_byo_provider(name: &str) -> Option<(ByoDialect, &str)> {
    let trimmed = name.trim();
    ByoDialect::ALL.into_iter().find_map(|dialect| {
        let prefix = dialect.prefix();
        let head = trimmed.get(..prefix.len())?;
        head.eq_ignore_ascii_case(prefix)
            .then(|| (dialect, &trimmed[prefix.len()..]))
    })
}

/// Classify a configured provider string exactly once.
pub(crate) fn classify_provider(name: &str) -> ProviderIdentity {
    #[cfg(feature = "wasm-plugins")]
    if name.trim().starts_with("wasm:") {
        return ProviderIdentity::WasmPlugin;
    }

    if let Some((dialect, raw_url)) = split_byo_provider(name) {
        return parse_custom_provider_url(raw_url, dialect.label(), dialect.format_hint()).map_or(
            ProviderIdentity::Unknown,
            |base_url| ProviderIdentity::ByoEndpoint { dialect, base_url },
        );
    }

    if let Some(info) = canonical_provider_info(name) {
        return ProviderIdentity::Catalog(info.name);
    }

    // The regional aliases are not catalog rows, but PRX's own credential
    // store folds them onto their vendor, so identity folds them the same way.
    // Which regional endpoint the name selects is carried by the key, not by
    // this fold: see `canonical_provider_key`.
    let lowered = name.trim().to_ascii_lowercase();
    canonical_china_provider_name(&lowered).map_or(ProviderIdentity::Unknown, |vendor| {
        ProviderIdentity::Catalog(canonical_provider_info(vendor).map_or(vendor, |info| info.name))
    })
}

/// The root-configured connection values, released to a selected provider
/// only when that provider is the one they were configured for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderConnection<'a> {
    pub api_key: Option<&'a str>,
    pub api_url: Option<&'a str>,
    /// Whether the root connection was released to the selection, i.e. the
    /// selection is the provider identity the root values were configured for.
    /// The rotation keys in `reliability.api_keys` follow this flag.
    pub released: bool,
}

/// The transport identity of a bring-your-own endpoint: the URL as the
/// provider will actually address it. `reqwest::Url` lowercases the scheme and
/// the host and normalizes the path; the trailing slash is dropped because
/// `OpenAiCompatibleProvider` drops it too.
fn normalized_endpoint_url(base_url: &str) -> String {
    reqwest::Url::parse(base_url).map_or_else(
        |_| base_url.trim().to_ascii_lowercase(),
        |url| url.as_str().trim_end_matches('/').to_string(),
    )
}

/// Canonical, comparable identity for a configured provider string: the raw
/// key of [`ProviderId::of`].
///
/// Catalog names and their aliases collapse to the catalog row, so
/// `claude-code` and `anthropic` are one provider; an alias that selects a
/// different regional endpoint keeps it, so `qwen-us` is not `qwen`; a
/// bring-your-own endpoint collapses to its dialect and normalized URL, so a
/// different port is a different provider; and anything unresolvable lives in
/// its own `unknown:` namespace, so it can never equal any of those.
pub(crate) fn canonical_provider_key(name: &str) -> String {
    ProviderId::of(name).as_str().to_owned()
}

/// Release the root-configured credential and base URL to a selected provider
/// if and only if the two names are the same provider identity.
///
/// Pure, and it never falls back: a provider that is not the configured one
/// gets nothing at all, so one vendor's secret can never be presented to
/// another. A selection that then has no credential of its own is refused by
/// the availability gate, before any transport exists.
pub(crate) fn configured_connection_for<'a>(
    selected: &str,
    configured: &str,
    api_key: Option<&'a str>,
    api_url: Option<&'a str>,
) -> ProviderConnection<'a> {
    if canonical_provider_key(selected) == canonical_provider_key(configured) {
        ProviderConnection {
            api_key,
            api_url,
            released: true,
        }
    } else {
        ProviderConnection {
            api_key: None,
            api_url: None,
            released: false,
        }
    }
}

/// The reliability settings a chain may be built with for `connection`.
///
/// `reliability.api_keys` are rotation credentials for the root `api_key`, so
/// they belong to the same provider identity and travel only with the root
/// connection. A selection the root connection was withheld from gets the same
/// settings with the rotation keys cleared; nothing else changes, and
/// `fallback_providers` stay because the factory builds every fallback without
/// a credential already.
pub(crate) fn scoped_reliability<'a>(
    reliability: &'a crate::config::ReliabilityConfig,
    connection: &ProviderConnection<'_>,
) -> std::borrow::Cow<'a, crate::config::ReliabilityConfig> {
    if connection.released || reliability.api_keys.is_empty() {
        std::borrow::Cow::Borrowed(reliability)
    } else {
        std::borrow::Cow::Owned(without_rotation_keys(reliability))
    }
}

/// The one credential a selected provider may receive.
///
/// `owned` pairs each credential in scope with the provider it was issued for.
/// The first credential whose owner is the selected provider's identity is
/// released; a credential issued for any other provider never is, so the
/// selection otherwise resolves its own (auth profile, or none for a keyless
/// endpoint).
pub(crate) fn credential_for_provider_identity<'a>(
    selected: &str,
    owned: &[(Option<&str>, Option<&'a str>)],
) -> Option<&'a str> {
    let selected_key = canonical_provider_key(selected);
    owned.iter().find_map(|(owner, credential)| {
        let credential = credential.map(str::trim).filter(|value| !value.is_empty())?;
        let owner = owner.map(str::trim).filter(|value| !value.is_empty())?;
        (canonical_provider_key(owner) == selected_key).then_some(credential)
    })
}

pub(super) fn without_rotation_keys(
    reliability: &crate::config::ReliabilityConfig,
) -> crate::config::ReliabilityConfig {
    crate::config::ReliabilityConfig {
        api_keys: Vec::new(),
        ..reliability.clone()
    }
}

/// PMM-B-2: one namespaced identity, and the canonicalizers folded into it.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderRuntimeOptions;

    const ROOT_SENTINEL: &str = "PMMB-ROOT-SENTINEL";
    const PROFILE_SENTINEL: &str = "PMMB-PROFILE-SENTINEL";

    /// The string `canonical_provider_key("qwen")` used to be, spelled back as
    /// a provider name. Nothing can construct it, so it classifies `Unknown`.
    const FORGED_QWEN: &str = "qwen@https://dashscope.aliyuncs.com/compatible-mode/v1";

    /// R-C1f G-2: an unresolvable spelling used to produce exactly the key of
    /// a catalog provider, so the connection configured for that provider was
    /// released to it.
    #[test]
    fn an_unresolvable_spelling_never_shares_a_catalog_providers_key_or_connection() {
        assert_eq!(classify_provider(FORGED_QWEN), ProviderIdentity::Unknown);
        assert_ne!(
            canonical_provider_key(FORGED_QWEN),
            canonical_provider_key("qwen"),
            "an Unknown spelling must not produce a catalog key"
        );
        let connection = configured_connection_for(FORGED_QWEN, "qwen", Some(ROOT_SENTINEL), None);
        assert!(
            !connection.released && connection.api_key.is_none(),
            "the root connection configured for qwen was released to an unresolvable spelling"
        );
    }

    /// `prx auth paste-token --provider <alias>` stores the token under
    /// `auth::normalize_provider(<alias>)`; the factory reads the store under
    /// its own fold of the provider name. The two folds must be one, or a
    /// token stored under an alias is never read.
    #[test]
    fn a_token_stored_under_an_alias_is_the_one_the_factory_reads() {
        for (typed, selected) in [
            ("google", "gemini"),
            ("grok", "xai"),
            ("kimi", "moonshot"),
            ("claude-cli", "anthropic"),
            ("GitHub-Copilot", "copilot"),
            ("Claude", "claude-code"),
            ("codex", "openai-codex"),
        ] {
            let temp = tempfile::TempDir::new().expect("test: temp dir");
            let auth = crate::auth::AuthService::new(temp.path(), false);
            let stored_under = crate::auth::normalize_provider(typed).expect("test: provider name");
            auth.store_provider_token(
                &stored_under,
                "default",
                PROFILE_SENTINEL,
                std::collections::HashMap::new(),
                true,
            )
            .expect("test: store token");
            let options = ProviderRuntimeOptions {
                openprx_dir: Some(temp.path().to_path_buf()),
                secrets_encrypt: false,
                ..ProviderRuntimeOptions::default()
            };
            assert_eq!(
                super::super::resolve_auth_profile_credential(selected, &options).as_deref(),
                Some(PROFILE_SENTINEL),
                "`--provider {typed}` stored the token under '{stored_under}', which '{selected}' does not read"
            );
        }
    }

    /// A decision service's credential is owned by its namespaced id in both
    /// folds, whichever spelling stored or reads it (CDP-02).
    #[test]
    fn a_decision_service_profile_key_is_the_namespaced_id_in_both_folds() {
        for spelling in ["typesafe", " TypeSafe ", "decision:typesafe"] {
            assert_eq!(
                canonical_auth_profile_provider(spelling),
                "decision:typesafe",
                "{spelling:?}"
            );
            assert_eq!(
                crate::auth::normalize_provider(spelling).expect("test: provider name"),
                "decision:typesafe",
                "{spelling:?}"
            );
        }
    }

    /// A bring-your-own endpoint keeps its literal (lowercased) spelling in
    /// both folds, so a profile for one is one the operator made on purpose.
    #[test]
    fn a_byo_profile_key_is_the_same_in_both_folds() {
        let spelling = " CUSTOM:http://H.invalid:8080/v1 ";
        assert_eq!(
            crate::auth::normalize_provider(spelling).expect("test: provider name"),
            canonical_auth_profile_provider(spelling)
        );
    }

    #[test]
    fn every_spelling_of_one_provider_is_one_namespaced_id() {
        for (spellings, key, namespace) in [
            (
                &["openai", " OpenAI "][..],
                "catalog:openai",
                ProviderNamespace::Catalog,
            ),
            (
                &["anthropic", "claude-code", "CLAUDE-CLI"][..],
                "catalog:anthropic",
                ProviderNamespace::Catalog,
            ),
            (
                &["qwen-us", "dashscope-us"][..],
                "catalog:qwen@https://dashscope-us.aliyuncs.com/compatible-mode/v1",
                ProviderNamespace::Catalog,
            ),
            (
                &["custom:http://H.invalid:8080/v1/", " CUSTOM:http://h.invalid:8080/v1"][..],
                "custom:http://h.invalid:8080/v1",
                ProviderNamespace::Custom,
            ),
            (
                &["anthropic-custom:https://h.invalid/v1"][..],
                "anthropic-custom:https://h.invalid/v1",
                ProviderNamespace::AnthropicCustom,
            ),
            (
                &["No-Such-Provider"][..],
                "unknown:no-such-provider",
                ProviderNamespace::Unknown,
            ),
            (
                &["custom:not a url"][..],
                "unknown:custom:not a url",
                ProviderNamespace::Unknown,
            ),
        ] {
            for spelling in spellings {
                let id = ProviderId::of(spelling);
                assert_eq!(id.as_str(), key, "{spelling}");
                assert_eq!(id.namespace(), namespace, "{spelling}");
                assert_eq!(id.is_resolvable(), namespace != ProviderNamespace::Unknown);
                assert_eq!(canonical_provider_key(spelling), key);
            }
        }
        assert_ne!(
            ProviderId::of("custom:http://h.invalid:8080/v1"),
            ProviderId::of("custom:http://h.invalid:8081/v1")
        );
        assert_ne!(ProviderId::of("qwen"), ProviderId::of("qwen-us"));
        assert_ne!(ProviderId::of(FORGED_QWEN), ProviderId::of("qwen"));
    }

    #[test]
    fn the_catalog_name_drops_the_region_and_exists_only_for_catalog_ids() {
        assert_eq!(ProviderId::of("qwen-us").catalog_name(), Some("qwen"));
        assert_eq!(ProviderId::of("kimi").catalog_name(), Some("moonshot"));
        assert_eq!(ProviderId::of("custom:http://h.invalid/v1").catalog_name(), None);
        assert_eq!(ProviderId::of("nope").catalog_name(), None);
    }

    #[test]
    fn local_runtime_identity_is_compiled_or_loopback_evidence() {
        assert!(ProviderId::of("ollama").is_local_runtime());
        assert!(ProviderId::of("mock").is_local_runtime());
        assert!(ProviderId::of("custom:http://127.0.0.1:9/v1").is_local_runtime());
        assert!(ProviderId::of("custom:http://localhost:9/v1").is_local_runtime());
        assert!(ProviderId::of("litellm").is_local_runtime_at(Some("http://127.0.0.1:9/v1")));
        assert!(!ProviderId::of("openai").is_local_runtime());
        assert!(!ProviderId::of("custom:https://example.com/v1").is_local_runtime());
    }

    #[test]
    fn an_unset_default_provider_has_one_owner() {
        assert_eq!(ProviderId::default_for_unset(), ProviderId::of("openrouter"));
        assert_eq!(
            ProviderId::default_for_unset().catalog_name(),
            Some(DEFAULT_PROVIDER_FOR_UNSET)
        );
    }

    #[test]
    fn a_provider_id_never_prints_url_userinfo() {
        let id = ProviderId::of("custom:http://alice:pmmb-fixture-pw@127.0.0.1:9/v1");
        assert!(
            id.as_str().contains("pmmb-fixture-pw"),
            "the key keeps userinfo for comparison"
        );
        for shown in [id.to_string(), format!("{id:?}")] {
            assert!(!shown.contains("pmmb-fixture-pw"), "{shown}");
            assert!(shown.contains("127.0.0.1:9"), "{shown}");
        }
    }

    /// R-PMMB F-4: the plugin index looks a provider up by its exported name,
    /// case-sensitively, so two spellings that differ in case are two plugins.
    #[cfg(feature = "wasm-plugins")]
    #[test]
    fn a_wasm_provider_id_keeps_the_exported_names_case() {
        assert_ne!(ProviderId::of("wasm:Foo"), ProviderId::of("wasm:foo"));
        assert_eq!(ProviderId::of(" wasm:Foo ").as_str(), "wasm:Foo");
        assert_eq!(ProviderId::of("wasm:Foo").namespace(), ProviderNamespace::Wasm);
    }

    /// R-PMMC F-6.5: the implicit store read finds a token pasted for a
    /// mixed-case `custom:` path under the key it was stored with, and never
    /// under another path's key.
    #[test]
    fn the_implicit_store_read_finds_a_mixed_case_byo_token_and_only_its_own() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let auth = crate::auth::AuthService::new(temp.path(), false);
        let stored = crate::auth::normalize_provider("custom:https://h.invalid/V1").expect("test: name");
        auth.store_provider_token(
            &stored,
            "default",
            PROFILE_SENTINEL,
            std::collections::HashMap::new(),
            true,
        )
        .expect("test: store");
        let options = ProviderRuntimeOptions {
            openprx_dir: Some(temp.path().to_path_buf()),
            secrets_encrypt: false,
            ..ProviderRuntimeOptions::default()
        };
        assert_eq!(
            super::super::resolve_auth_profile_credential("custom:https://H.invalid/V1/", &options).as_deref(),
            Some(PROFILE_SENTINEL),
            "every spelling of the identity finds its token"
        );
        assert_eq!(
            super::super::resolve_auth_profile_credential("custom:https://h.invalid/v1", &options),
            None,
            "another path never receives it"
        );

        let legacy = tempfile::TempDir::new().expect("test: temp dir");
        crate::auth::AuthService::new(legacy.path(), false)
            .store_provider_token(
                "custom:https://h.invalid/v1",
                "default",
                PROFILE_SENTINEL,
                std::collections::HashMap::new(),
                true,
            )
            .expect("test: store");
        let options = ProviderRuntimeOptions {
            openprx_dir: Some(legacy.path().to_path_buf()),
            secrets_encrypt: false,
            ..ProviderRuntimeOptions::default()
        };
        assert_eq!(
            super::super::resolve_auth_profile_credential("custom:https://H.INVALID/v1", &options).as_deref(),
            Some(PROFILE_SENTINEL),
            "a token stored lowercased before N-1 is still read for its own identity"
        );
        assert_eq!(
            super::super::resolve_auth_profile_credential("custom:https://h.invalid/V1", &options),
            None,
            "a lowercased legacy key is not another path's token"
        );
    }
}
