//! The one place a surface checks a provider/model selection before it
//! builds anything.
//!
//! Every surface that selects a model — chat startup and `/model`, a channel
//! `/models` or `/model` switch, the gateway turn runtime, delegate,
//! `sessions_spawn`, the session worker, cron and xin, the evolution judge,
//! onboarding — calls [`check`] (or [`check_with`]) first. It resolves the
//! selection through [`catalog::resolve`] against the operator declarations
//! and the cached catalog metadata, so a model that cannot run the request is
//! refused before any provider exists and nothing is sent anywhere.
//!
//! Policy (D-018 Q1): a surface running an existing selection passes
//! [`UnknownPolicy::Invocation`], so a model known by id only keeps running
//! and its warning is logged once here; a surface recording a new durable
//! selection passes [`UnknownPolicy::DurableSelection`].

use super::catalog::{
    self, CatalogView, DecisionAdapterKind, ModelOperation, Requirement, ResolutionError, ResolvedModel, UnknownPolicy,
    decision_adapter,
};
use super::credentials::{ConfigCredentialSource, CredentialRef, LegacySlot};
use super::{DEFAULT_PROVIDER_FOR_UNSET, ProviderId};
use crate::config::Config;
use crate::decision::model_pin::is_unpinned_model;

/// The model a surface runs when neither the selection nor `default_model`
/// names one.
pub const DEFAULT_MODEL_FOR_UNSET: &str = "anthropic/claude-sonnet-4";

/// The configured default provider, or the one an unset `default_provider`
/// means.
#[must_use]
pub fn default_provider(config: &Config) -> &str {
    config.default_provider.as_deref().unwrap_or(DEFAULT_PROVIDER_FOR_UNSET)
}

/// The configured default model, or the one an unset `default_model` means.
#[must_use]
pub fn default_model(config: &Config) -> &str {
    config.default_model.as_deref().unwrap_or(DEFAULT_MODEL_FOR_UNSET)
}

/// The evidence a surface resolves against: the operator declarations in
/// `config` and the catalog metadata cached under its workspace. An
/// unreadable cache is reported and left out; it only ever adds evidence.
#[must_use]
pub fn catalog_for(config: &Config) -> CatalogView {
    let catalog = CatalogView::from_config(config);
    match catalog::discovery::cached_descriptors(&config.workspace_dir) {
        Ok(discovered) => catalog.with_discovered(discovered),
        Err(error) => {
            tracing::warn!(
                error = %crate::security::redaction::redact_provider_text(&error.to_string()),
                "model cache could not be read; resolving without discovered metadata"
            );
            catalog
        }
    }
}

/// The credential a selection of `provider` uses when nothing more specific
/// is configured for it: the root connection (`api_key`, `api_url` and
/// `reliability.api_keys`, as one unit) when `provider` is the configured
/// default provider's identity and the root holds something, and otherwise
/// the provider's own store, so no configured credential is released to it.
#[must_use]
pub fn credential_for(config: &Config, provider: &str) -> CredentialRef {
    let root_holds_material = [config.api_key.as_deref(), config.api_url.as_deref()]
        .into_iter()
        .flatten()
        .any(|value| !value.trim().is_empty())
        || config.reliability.api_keys.iter().any(|key| !key.trim().is_empty());
    if root_holds_material && ProviderId::of(provider) == ProviderId::of(default_provider(config)) {
        CredentialRef::LegacyInline(LegacySlot::Root)
    } else {
        CredentialRef::ProviderStore
    }
}

/// Resolve `provider`/`model` for `requirement` with the credential
/// [`credential_for`] picks.
///
/// # Errors
///
/// The resolver's refusal; nothing has been constructed or sent.
pub fn check(
    config: &Config,
    provider: &str,
    model: &str,
    requirement: &Requirement,
    policy: UnknownPolicy,
) -> Result<ResolvedModel, ResolutionError> {
    check_with(
        config,
        &catalog_for(config),
        provider,
        model,
        credential_for(config, provider),
        requirement,
        policy,
    )
}

/// Resolve with an explicit catalog and credential reference (an agent's or
/// a route's own key).
///
/// # Errors
///
/// The resolver's refusal; nothing has been constructed or sent.
pub fn check_with(
    config: &Config,
    catalog: &CatalogView,
    provider: &str,
    model: &str,
    credential: CredentialRef,
    requirement: &Requirement,
    policy: UnknownPolicy,
) -> Result<ResolvedModel, ResolutionError> {
    // A `hint:<route>` model is the route's destination, with the route's
    // own key when it has one.
    let (provider, model, credential) = if let Some(route) = route_target(catalog, provider, model)? {
        let route_credential = if route.has_own_key {
            CredentialRef::LegacyInline(LegacySlot::ModelRoute(route.hint.clone()))
        } else if let Some(id) = &route.credential_ref {
            CredentialRef::Profile { id: id.clone() }
        } else if ProviderId::of(provider) == ProviderId::of(&route.provider) {
            // A named agent's own connection remains scoped to that agent
            // when its selected route stays on the same provider. A route to
            // another identity must resolve that destination separately.
            credential
        } else {
            credential_for(config, &route.provider)
        };
        (route.provider.as_str(), route.model.as_str(), route_credential)
    } else {
        (provider, model, credential)
    };
    let resolved = catalog::resolve(
        &catalog::Selection {
            provider,
            model,
            credential,
        },
        requirement,
        catalog,
        &ConfigCredentialSource::new(config),
        policy,
    )?;
    if let Some(warning) = resolved.warning() {
        tracing::warn!(target: "providers::selection", "{warning}");
    }
    Ok(resolved)
}

/// Check a decision backend selection (D-036 item 4; D-040 OQ-A2).
///
/// A model is selectable as a decision backend only when its descriptor
/// declares the decision endpoint, whoever selects it: the check always
/// runs [`UnknownPolicy::DurableSelection`] for
/// [`ModelOperation::DecisionEndpoint`], at plan, factory build and status
/// alike, so an operator selection is no more lenient than a model's. A
/// hosted decision service (TypeSafe, OpenRouter) also needs a pinned model
/// ([`crate::decision::model_pin::is_unpinned_model`]) and an auth-profile
/// credential: never the root inline key, another inline slot or the
/// provider's own store (D-040 OQ-A4, D-074). The credential is released by
/// the resolver's owner rule; `CredentialRef::None` is accepted only where
/// the adapter needs no credential (a `custom:` endpoint), and the decision
/// credential resolver (CDP-05) passes it only for a literal-loopback
/// endpoint or an explicit `credential_ref = "none"`.
///
/// # Errors
///
/// [`ResolutionError::DecisionModelNotPinned`] for an unpinned hosted model,
/// [`ResolutionError::DecisionCredentialNotProfile`] for a hosted service
/// referencing anything but an auth profile, otherwise the resolver's
/// refusal; nothing has been constructed or sent.
pub fn check_decision_backend(
    config: &Config,
    catalog: &CatalogView,
    provider: &str,
    model: &str,
    credential: CredentialRef,
) -> Result<ResolvedModel, ResolutionError> {
    hosted_decision_rules(provider, model, &credential)?;
    catalog::resolve(
        &catalog::Selection {
            provider,
            model,
            credential,
        },
        &Requirement::operation(ModelOperation::DecisionEndpoint),
        catalog,
        &ConfigCredentialSource::new(config),
        UnknownPolicy::DurableSelection,
    )
}

/// The hosted decision service rules of [`check_decision_backend`]: a
/// pinned model and an auth-profile credential.
fn hosted_decision_rules(provider: &str, model: &str, credential: &CredentialRef) -> Result<(), ResolutionError> {
    if !decision_adapter(&ProviderId::of(provider)).is_some_and(DecisionAdapterKind::is_hosted) {
        return Ok(());
    }
    let shown = || super::redact_endpoint_userinfo(provider.trim()).into_owned();
    if is_unpinned_model(model) {
        return Err(ResolutionError::DecisionModelNotPinned {
            provider: shown(),
            model: model.trim().to_string(),
        });
    }
    if !matches!(credential, CredentialRef::Profile { .. }) {
        return Err(ResolutionError::DecisionCredentialNotProfile {
            provider: shown(),
            reference: credential.to_string(),
        });
    }
    Ok(())
}

/// The rules of [`check_decision_backend`] that read the configuration
/// alone: the hosted rules and the model's declaration for the decision
/// endpoint, without resolving the credential. A disabled decision plane is
/// checked with these only, so it never opens the credential store (D-082
/// N-9); enabling it runs the whole check.
///
/// # Errors
///
/// As [`check_decision_backend`], less the credential's resolution.
pub fn check_decision_model(
    catalog: &CatalogView,
    provider: &str,
    model: &str,
    credential: &CredentialRef,
) -> Result<(), ResolutionError> {
    hosted_decision_rules(provider, model, credential)?;
    catalog::resolver::resolve_model(
        provider,
        model,
        &Requirement::operation(ModelOperation::DecisionEndpoint),
        catalog,
        UnknownPolicy::DurableSelection,
    )
    .map(|_| ())
}

/// The `[[model_routes]]` destination a `hint:<route>` model names, or
/// `None` when `model` is not a hint. A hint no route carries is refused,
/// naming the configured hints; a route whose own model is a hint is not
/// followed (the router does not chain hints either).
///
/// # Errors
///
/// When the hint names no configured route.
pub fn route_target<'c>(
    catalog: &'c CatalogView,
    provider: &str,
    model: &str,
) -> Result<Option<&'c catalog::RouteTarget>, ResolutionError> {
    let Some(hint) = model.trim().strip_prefix("hint:") else {
        return Ok(None);
    };
    let shown = || crate::security::redaction::redact_provider_text(provider.trim());
    // The router looks the hint up exactly, so a spelling with whitespace
    // would pass here and never route (D-035).
    let problem = if hint.is_empty() {
        Some("is empty")
    } else if hint.chars().any(char::is_whitespace) {
        Some("contains whitespace")
    } else {
        None
    };
    if let Some(problem) = problem {
        return Err(ResolutionError::MalformedRouteHint {
            provider: shown(),
            hint: hint.to_string(),
            problem,
        });
    }
    let hints = catalog.route_hints();
    let route = catalog.route(hint).ok_or_else(|| ResolutionError::UnknownRouteHint {
        provider: shown(),
        hint: hint.to_string(),
        configured: if hints.is_empty() {
            "none".to_string()
        } else {
            hints.join(", ")
        },
    })?;
    // The router sends a route's model literally, so a route to another hint
    // never resolves (D-035).
    if route.model.trim().starts_with("hint:") {
        return Err(ResolutionError::UnresolvedHint {
            provider: crate::security::redaction::redact_provider_text(&route.provider),
            model: route.model.clone(),
        });
    }
    Ok(Some(route))
}

/// Trim the model names a configuration change sets (R-PMMCh C-2): the
/// default, and each model route, agent binding and embedding route that is
/// not already present unchanged in `current`. The resolver trims what it
/// checks, so the stored name must be the one it resolved. Entries the
/// change leaves alone are not rewritten.
pub fn trim_changed_model_names(current: &Config, patched: &mut Config) {
    fn trim_in_place(model: &mut String) {
        let trimmed = model.trim();
        if trimmed.len() != model.len() {
            *model = trimmed.to_string();
        }
    }

    if patched.default_model != current.default_model
        && let Some(model) = patched.default_model.as_mut()
    {
        trim_in_place(model);
    }
    for route in &mut patched.model_routes {
        let unchanged = current.model_routes.iter().any(|before| {
            before.hint == route.hint && before.provider == route.provider && before.model == route.model
        });
        if !unchanged {
            trim_in_place(&mut route.model);
        }
    }
    for (name, agent) in &mut patched.agents {
        let unchanged = current
            .agents
            .get(name)
            .is_some_and(|before| before.provider == agent.provider && before.model == agent.model);
        if !unchanged {
            trim_in_place(&mut agent.model);
        }
    }
    for route in &mut patched.embedding_routes {
        let unchanged = current.embedding_routes.iter().any(|before| {
            before.hint == route.hint && before.provider == route.provider && before.model == route.model
        });
        if !unchanged {
            trim_in_place(&mut route.model);
        }
    }
}

/// Warn, loudly, when a `hint:<route>` model is selected on a surface that
/// does not build a router (the gateway, the channel runtime, `delegate`,
/// `sessions_spawn`, the session worker; D-035). The selection is accepted —
/// nothing fails at start-up and nothing restarts — but every request on
/// that surface will be refused before transport, because its provider chain
/// cannot resolve the hint. Returns the warning for surfaces that also answer
/// a user (a channel `/model`).
#[must_use]
pub fn warn_if_hint_unroutable(surface: &str, model: &str) -> Option<String> {
    let model = model.trim();
    if !model.starts_with("hint:") {
        return None;
    }
    let warning = format!(
        "model '{model}' is a routing hint, and {surface} does not route hints: requests on this surface will be \
         refused. Hints route in `prx chat`, `prx agent` (and `prx go --message`), cron and xin; select the \
         route's model directly here."
    );
    tracing::warn!(target: "providers::selection", "{warning}");
    Some(warning)
}

/// Resolve `provider`/`model` for `requirement` without releasing a
/// credential: for a surface whose credential is decided by the same owner
/// rule elsewhere (a delegate agent's own key, a spawned child's key), so it
/// still refuses a model that cannot run the request before any provider
/// exists. An unverified model's warning is logged here.
///
/// # Errors
///
/// The resolver's refusal; nothing has been constructed or sent.
pub fn check_model(
    catalog: &CatalogView,
    provider: &str,
    model: &str,
    requirement: &Requirement,
    policy: UnknownPolicy,
) -> Result<(), ResolutionError> {
    let unverified = check_model_unproven(catalog, provider, model, requirement, policy)?;
    if let Some(unverified) = unverified {
        let UnverifiedModel {
            provider,
            model,
            unproven,
            evidence,
        } = unverified;
        tracing::warn!(
            target: "providers::selection",
            provider = %crate::security::redaction::redact_provider_text(provider.trim()),
            model = %model.trim(),
            unverified = %unproven.join(", "),
            evidence = %evidence,
            "model is unverified for this request; it runs, but PRX cannot confirm it supports it"
        );
    }
    Ok(())
}

/// A model a selection resolved to that the evidence does not verify for
/// every requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnverifiedModel {
    /// The provider it resolved to (after a `hint:` route), as configured.
    pub provider: String,
    /// The model it resolved to.
    pub model: String,
    /// The requirements without evidence.
    pub unproven: Vec<&'static str>,
    /// Where the evidence came from.
    pub evidence: String,
}

/// The one model check every surface shares (the pre-build checks here and
/// the configuration transaction's validation): follow a `hint:` route,
/// then resolve the model for `requirement` under `policy`. `Ok(Some(..))`
/// when it is accepted unverified.
///
/// # Errors
///
/// The resolver's refusal; nothing has been constructed or sent.
pub fn check_model_unproven(
    catalog: &CatalogView,
    provider: &str,
    model: &str,
    requirement: &Requirement,
    policy: UnknownPolicy,
) -> Result<Option<UnverifiedModel>, ResolutionError> {
    let (provider, model) = route_target(catalog, provider, model)?.map_or((provider, model), |route| {
        (route.provider.as_str(), route.model.as_str())
    });
    let (_, descriptor, unproven) = catalog::resolver::resolve_model(provider, model, requirement, catalog, policy)?;
    Ok((!unproven.is_empty()).then(|| UnverifiedModel {
        provider: provider.to_string(),
        model: model.trim().to_string(),
        unproven,
        evidence: descriptor.source.to_string(),
    }))
}

/// Whether `provider` names one of PRX's own embedders (`local`, `none`, or
/// nothing), which are not providers and always resolve.
#[must_use]
pub fn is_prx_embedder(provider: &str) -> bool {
    let provider = provider.trim();
    provider.is_empty() || provider.eq_ignore_ascii_case("local") || provider.eq_ignore_ascii_case("none")
}

/// [`check_embedder`] under an explicit policy, returning the unverified
/// model instead of logging it.
///
/// # Errors
///
/// The resolver's refusal; nothing has been constructed or sent.
pub fn check_embedder_unproven(
    catalog: &CatalogView,
    provider: &str,
    model: &str,
    policy: UnknownPolicy,
) -> Result<Option<UnverifiedModel>, ResolutionError> {
    if is_prx_embedder(provider) {
        return Ok(None);
    }
    check_model_unproven(
        catalog,
        provider.trim(),
        model,
        &Requirement::operation(catalog::ModelOperation::Embedding),
        policy,
    )
}

/// Resolve an embedder selection for embeddings. `local` (PRX's hash
/// embedder) and `none` are PRX's own embedders, not providers, and always
/// resolve; anything else is resolved against `catalog` like a chat
/// selection, so a provider without an embedding adapter, or a model the
/// evidence rules out for embeddings, is refused before it is built.
///
/// # Errors
///
/// The resolver's refusal; nothing has been constructed or sent.
pub fn check_embedder(catalog: &CatalogView, provider: &str, model: &str) -> Result<(), ResolutionError> {
    if is_prx_embedder(provider) {
        return Ok(());
    }
    check_model(
        catalog,
        provider.trim(),
        model,
        &Requirement::operation(catalog::ModelOperation::Embedding),
        UnknownPolicy::Invocation,
    )
}

/// Resolve the configured default provider and model for a chat request.
///
/// # Errors
///
/// The resolver's refusal; nothing has been constructed or sent.
pub fn check_default(config: &Config, policy: UnknownPolicy) -> Result<ResolvedModel, ResolutionError> {
    check(
        config,
        default_provider(config),
        default_model(config),
        &Requirement::chat(),
        policy,
    )
}

/// Resolve every `[[model_routes]]` destination for chat with its own key,
/// so a route to a model that cannot chat is refused where the routed chain
/// is built rather than on the first turn that routes to it.
///
/// # Errors
///
/// The first refused route, named by its hint.
pub fn check_routes(config: &Config, catalog: &CatalogView, policy: UnknownPolicy) -> anyhow::Result<()> {
    for route in &config.model_routes {
        if route.model.trim().starts_with("hint:") {
            return Err(anyhow::anyhow!(
                "model route `hint:{}` is refused: its model '{}' is itself a routing hint, which the router sends \
                 literally; name a model",
                route.hint,
                route.model.trim()
            ));
        }
        let credential = if route.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) {
            CredentialRef::LegacyInline(LegacySlot::ModelRoute(route.hint.clone()))
        } else if let Some(id) = route
            .credential_ref
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            CredentialRef::Profile { id: id.to_string() }
        } else {
            credential_for(config, &route.provider)
        };
        check_with(
            config,
            catalog,
            &route.provider,
            &route.model,
            credential,
            &Requirement::chat(),
            policy,
        )
        .map_err(|refusal| anyhow::anyhow!("model route `hint:{}` is refused: {refusal}", route.hint))?;
    }
    Ok(())
}

/// How the configured default resolves, in one line for `prx doctor` and
/// `prx models list`: `Ok` with the line when it is verified or only
/// unverified (the line then says so), `Err` with the refusal otherwise.
///
/// # Errors
///
/// The resolver's refusal, as a sentence.
pub fn default_status(config: &Config) -> Result<(bool, String), String> {
    let provider = crate::security::redaction::redact_provider_text(default_provider(config));
    let model = default_model(config);
    match check(
        config,
        default_provider(config),
        model,
        &Requirement::chat(),
        UnknownPolicy::Invocation,
    ) {
        Ok(resolved) => Ok(match resolved.verification() {
            catalog::Verification::Verified => (
                true,
                format!(
                    "default model '{model}' on provider '{provider}' is verified for chat (evidence: {})",
                    resolved.source()
                ),
            ),
            catalog::Verification::Unverified { unproven } => (
                false,
                format!(
                    "default model '{model}' on provider '{provider}' runs unverified for {} (evidence: {}); \
                     declare it in `[[providers.models]]` or refresh the catalog with `prx models refresh`",
                    unproven.join(", "),
                    resolved.source()
                ),
            ),
        }),
        Err(refusal) => Err(format!("default model '{model}' is refused: {refusal}")),
    }
}

#[cfg(test)]
mod tests {
    /// RED before PMME-h (D-095 N-1): `default_status` (`prx config status`,
    /// `prx models default show`, `prx models list`, doctor) showed a
    /// provider URL's query values; only userinfo was redacted.
    #[test]
    fn default_status_redacts_provider_url_query_values() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let config = config_in(
            &temp,
            "custom:http://127.0.0.1:1/v1?tenant=sentinel-status-tenant",
            "local-model",
        );
        let line = match default_status(&config) {
            Ok((_, line)) | Err(line) => line,
        };
        assert!(!line.contains("sentinel-status-tenant"), "{line}");
        assert!(line.contains("?tenant=[redacted]"), "{line}");
    }

    use super::*;
    use crate::providers::catalog::ModelOperation;

    fn config_in(temp: &tempfile::TempDir, provider: &str, model: &str) -> Config {
        let mut config = Config::default();
        config.workspace_dir = temp.path().to_path_buf();
        config.config_path = temp.path().join("config.toml");
        config.default_provider = Some(provider.to_string());
        config.default_model = Some(model.to_string());
        config.api_key = Some("PMMC-SELECTION-ROOT".to_string());
        config.secrets.encrypt = false;
        config
    }

    #[test]
    fn the_root_connection_goes_only_to_the_default_identity_and_only_when_it_exists() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let mut config = config_in(&temp, "anthropic", "claude-sonnet-4");
        assert_eq!(
            credential_for(&config, "claude-code"),
            CredentialRef::LegacyInline(LegacySlot::Root),
            "an alias of the default is the default"
        );
        assert_eq!(credential_for(&config, "openai"), CredentialRef::ProviderStore);
        config.api_key = None;
        assert_eq!(
            credential_for(&config, "anthropic"),
            CredentialRef::ProviderStore,
            "an empty root falls back to the provider's own store"
        );
        let resolved = check(
            &config_in(&temp, "anthropic", "claude-sonnet-4"),
            "claude-code",
            "claude-sonnet-4",
            &Requirement::chat(),
            UnknownPolicy::Invocation,
        )
        .expect("test: resolves");
        assert_eq!(resolved.credential().api_key(), Some("PMMC-SELECTION-ROOT"));
    }

    #[test]
    fn a_route_to_a_model_that_cannot_chat_is_refused_by_hint() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let mut config = config_in(&temp, "openai", "gpt-4o");
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "judge".to_string(),
            provider: "openrouter".to_string(),
            model: "vendor/decision".to_string(),
            api_key: Some("PMMC-ROUTE-KEY".to_string()),
            credential_ref: None,
        });
        let catalog = catalog_for(&config).with_curated([catalog::CuratedModel {
            operations: Some(vec![ModelOperation::DecisionEndpoint]),
            ..catalog::CuratedModel::new(ProviderId::of("openrouter"), Some("vendor/decision"))
        }]);
        let error = check_routes(&config, &catalog, UnknownPolicy::Invocation).expect_err("refused");
        let text = format!("{error:#}");
        assert!(
            text.contains("hint:judge") && text.contains("cannot be used for chat"),
            "{text}"
        );
        assert!(!text.contains("PMMC-ROUTE-KEY"), "{text}");
        check_routes(&config, &catalog_for(&config), UnknownPolicy::Invocation).expect("an id-only route runs");
    }

    /// A catalog that declares `model` of `provider` for the given
    /// operations only.
    fn declared(config: &Config, provider: &str, model: &str, operations: Vec<ModelOperation>) -> CatalogView {
        catalog_for(config).with_curated([catalog::CuratedModel {
            operations: Some(operations),
            ..catalog::CuratedModel::new(ProviderId::of(provider), Some(model))
        }])
    }

    /// CDP-02 (A02 RB-A): a decision model declared on a `custom:` endpoint
    /// resolves for the decision endpoint. Before the decision bound it was
    /// `OperationUnsupported("no adapter …")`.
    #[test]
    fn a_declared_decision_model_on_a_custom_endpoint_resolves_for_decision() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let config = config_in(&temp, "openai", "gpt-4o");
        let endpoint = "custom:http://127.0.0.1:8765/v1";
        let catalog = declared(&config, endpoint, "jev-1.13.0", vec![ModelOperation::DecisionEndpoint]);
        let resolved = check_decision_backend(&config, &catalog, endpoint, "jev-1.13.0", CredentialRef::None)
            .expect("test: a declared decision model resolves");
        assert_eq!(resolved.operation(), ModelOperation::DecisionEndpoint);
        assert_eq!(resolved.provider(), &ProviderId::of(endpoint));
        assert_eq!(resolved.credential().api_key(), None);
    }

    /// V2.10: a chat-only descriptor is refused as a decision backend before
    /// any transport exists, and a model with no declaration is refused as
    /// unverified: D-040 OQ-A2 makes every selection strict, whoever asks.
    #[test]
    fn a_chat_only_or_undeclared_model_is_refused_as_a_decision_backend() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let config = config_in(&temp, "openai", "gpt-4o");
        let endpoint = "custom:http://127.0.0.1:8765/v1";
        let chat_only = declared(&config, endpoint, "chatty", vec![ModelOperation::Chat]);
        let error = check_decision_backend(&config, &chat_only, endpoint, "chatty", CredentialRef::None)
            .expect_err("a chat-only model is no decision backend");
        assert!(
            matches!(
                error,
                ResolutionError::OperationUnsupported {
                    operation: ModelOperation::DecisionEndpoint,
                    ..
                }
            ),
            "{error}"
        );
        let error = check_decision_backend(
            &config,
            &catalog_for(&config),
            endpoint,
            "undeclared",
            CredentialRef::None,
        )
        .expect_err("an undeclared model is not selectable");
        assert!(
            matches!(
                error,
                ResolutionError::OperationUnverified {
                    operation: ModelOperation::DecisionEndpoint,
                    ..
                }
            ),
            "{error}"
        );
        for provider in ["anthropic-custom:http://127.0.0.1:8765/v1", "anthropic", "gemini"] {
            let catalog = declared(&config, provider, "m", vec![ModelOperation::DecisionEndpoint]);
            assert!(
                matches!(
                    check_decision_backend(&config, &catalog, provider, "m", CredentialRef::ProviderStore),
                    Err(ResolutionError::OperationUnsupported { .. })
                ),
                "{provider} has no decision transport, whatever it declares"
            );
        }
    }

    /// A hosted decision service needs a pinned model: an empty model, an
    /// alias or a `latest` spelling is refused before anything else.
    #[test]
    fn a_hosted_alias_or_empty_model_is_refused() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let config = config_in(&temp, "openai", "gpt-4o");
        for provider in ["typesafe", "openrouter"] {
            for model in [
                "",
                "  ",
                "latest",
                "AUTO",
                "default",
                "stable",
                "openrouter/auto",
                "jev:latest",
                "jev-latest",
            ] {
                let error = check_decision_backend(
                    &config,
                    &catalog_for(&config),
                    provider,
                    model,
                    CredentialRef::ProviderStore,
                )
                .expect_err("an unpinned hosted model is refused");
                assert!(
                    matches!(error, ResolutionError::DecisionModelNotPinned { .. }),
                    "{provider} {model:?}: {error}"
                );
            }
        }
        // A pinned model with a profile passes both hosted checks and meets
        // the strict operation check next: undeclared, so unverified.
        let error = check_decision_backend(
            &config,
            &catalog_for(&config),
            "typesafe",
            "jev-1.13.0",
            CredentialRef::Profile {
                id: "decisions".to_string(),
            },
        )
        .expect_err("an undeclared hosted model is not selectable");
        assert!(matches!(error, ResolutionError::OperationUnverified { .. }), "{error}");
    }

    /// The credential rule holds for the decision service: a profile stored
    /// for `openai` is not released to `decision:typesafe`, and a profile
    /// stored for `typesafe` is.
    #[test]
    fn a_profile_owned_by_openai_is_not_released_to_decision_typesafe() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let auth = crate::auth::AuthService::new(temp.path(), false);
        let openai = auth
            .store_provider_token(
                "openai",
                "work",
                "CDP02-OPENAI-SENTINEL",
                std::collections::HashMap::new(),
                false,
            )
            .expect("test: store");
        let typesafe = auth
            .store_provider_token(
                "typesafe",
                "decisions",
                "CDP02-TYPESAFE-SENTINEL",
                std::collections::HashMap::new(),
                false,
            )
            .expect("test: store");
        let config = config_in(&temp, "openai", "gpt-4o");
        let catalog = declared(
            &config,
            "typesafe",
            "jev-1.13.0",
            vec![ModelOperation::DecisionEndpoint],
        );
        let error = check_decision_backend(
            &config,
            &catalog,
            "typesafe",
            "jev-1.13.0",
            CredentialRef::Profile { id: openai.id },
        )
        .expect_err("another provider's profile is not released");
        assert!(
            matches!(error, ResolutionError::CredentialOwnerMismatch { .. }),
            "{error}"
        );
        assert!(!error.to_string().contains("CDP02-OPENAI-SENTINEL"));
        let resolved = check_decision_backend(
            &config,
            &catalog,
            "decision:typesafe",
            "jev-1.13.0",
            CredentialRef::Profile { id: typesafe.id },
        )
        .expect("test: the service's own profile is released");
        assert_eq!(resolved.credential().api_key(), Some("CDP02-TYPESAFE-SENTINEL"));
    }

    /// D-040 OQ-A4, D-074: a hosted decision service uses only an auth
    /// profile. The root inline key, any other inline slot, the provider's
    /// own store and no credential at all are refused before resolution, and
    /// the refusal names the reference, never a key. A `custom:` endpoint is
    /// not hosted and keeps the resolver's rule.
    #[test]
    fn a_hosted_decision_service_accepts_only_a_profile_credential() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let mut config = config_in(&temp, "openrouter", "vendor/chat-model");
        config.api_key = Some("CDP02F-ROOT-SENTINEL".to_string());
        for provider in ["typesafe", "decision:typesafe", "openrouter"] {
            let catalog = declared(&config, provider, "jev-1.13.0", vec![ModelOperation::DecisionEndpoint]);
            for credential in [
                CredentialRef::LegacyInline(LegacySlot::Root),
                CredentialRef::ProviderStore,
                CredentialRef::None,
            ] {
                let shown = credential.to_string();
                let error = check_decision_backend(&config, &catalog, provider, "jev-1.13.0", credential)
                    .expect_err("only a profile is accepted");
                assert!(
                    matches!(error, ResolutionError::DecisionCredentialNotProfile { .. }),
                    "{provider} {shown}: {error}"
                );
                let message = error.to_string();
                assert!(message.contains(&shown), "{message}");
                assert!(!message.contains("CDP02F-ROOT-SENTINEL"), "{message}");
            }
        }
        let endpoint = "custom:http://127.0.0.1:8765/v1";
        let catalog = declared(
            &config,
            endpoint,
            "local-decider",
            vec![ModelOperation::DecisionEndpoint],
        );
        check_decision_backend(&config, &catalog, endpoint, "local-decider", CredentialRef::None)
            .expect("a compatible endpoint is not hosted");
    }

    fn hinted(temp: &tempfile::TempDir) -> Config {
        let mut config = config_in(temp, "openai", "hint:fast");
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "fast".to_string(),
            provider: "openrouter".to_string(),
            model: "vendor/fast-chat".to_string(),
            api_key: None,
            credential_ref: None,
        });
        config
    }

    /// R-PMMC F-1: a `hint:<route>` model is resolved through
    /// `[[model_routes]]` before the resolver sees it, on every entry point.
    #[test]
    fn a_hint_model_resolves_through_its_route_everywhere() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let config = hinted(&temp);
        check_default(&config, UnknownPolicy::Invocation).expect("a hint default resolves through its route");
        check(
            &config,
            "openai",
            "hint:fast",
            &Requirement::chat(),
            UnknownPolicy::Invocation,
        )
        .expect("chat /model");
        check_model(
            &catalog_for(&config),
            "openai",
            "hint:fast",
            &Requirement::chat(),
            UnknownPolicy::Invocation,
        )
        .expect("delegate/spawn");
        assert!(
            default_status(&config).is_ok(),
            "doctor reports a hint default as running"
        );

        let error = check(
            &config,
            "openai",
            "hint:slow",
            &Requirement::chat(),
            UnknownPolicy::Invocation,
        )
        .expect_err("an unknown hint is refused");
        assert!(
            error.to_string().contains("hint:slow") && error.to_string().contains("fast"),
            "{error}"
        );

        let mut decision = hinted(&temp);
        decision.providers.models.push(
            toml::from_str(
                "provider = \"openrouter\"\nmodel = \"vendor/fast-chat\"\noperations = [\"decision_endpoint\"]\n",
            )
            .expect("test: declaration"),
        );
        let error =
            check_default(&decision, UnknownPolicy::Invocation).expect_err("the route's model is what is checked");
        assert!(error.to_string().contains("vendor/fast-chat"), "{error}");
    }

    fn route(hint: &str, provider: &str, model: &str) -> crate::config::ModelRouteConfig {
        crate::config::ModelRouteConfig {
            hint: hint.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            api_key: None,
            credential_ref: None,
        }
    }

    /// Each hint resolves its own stored credential even when both routes
    /// share one provider identity. Missing or foreign references stop before
    /// the loopback endpoint receives a request.
    #[tokio::test]
    async fn two_hints_on_one_provider_keep_distinct_profile_keys_on_wire() {
        use crate::providers::credential_probe::{CredentialProbe, PROBE_MODEL};
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let probe = CredentialProbe::start().await;
        let destination = probe.custom_provider();
        let auth = crate::auth::AuthService::new(temp.path(), false);
        let first = auth
            .store_provider_token(
                &destination,
                "route-a",
                "PRX-HINT-ROUTE-A",
                std::collections::HashMap::new(),
                false,
            )
            .expect("test: first profile");
        let second = auth
            .store_provider_token(
                &destination,
                "route-b",
                "PRX-HINT-ROUTE-B",
                std::collections::HashMap::new(),
                false,
            )
            .expect("test: second profile");
        let foreign = auth
            .store_provider_token(
                "openai",
                "foreign",
                "PRX-HINT-FOREIGN",
                std::collections::HashMap::new(),
                false,
            )
            .expect("test: foreign profile");
        let mut config = config_in(&temp, "openai", "hint:a");
        config.model_routes = vec![
            route("a", &destination, PROBE_MODEL),
            route("b", &destination, PROBE_MODEL),
        ];
        config.model_routes.get_mut(0).expect("first route").credential_ref = Some(first.id);
        config.model_routes.get_mut(1).expect("second route").credential_ref = Some(second.id);
        let catalog = catalog_for(&config);
        let mut reliability = config.reliability.clone();
        reliability.fallback_providers.clear();
        reliability.provider_retries = 0;
        for (hint, key) in [("a", "PRX-HINT-ROUTE-A"), ("b", "PRX-HINT-ROUTE-B")] {
            let resolved = check_with(
                &config,
                &catalog,
                "openai",
                &format!("hint:{hint}"),
                credential_for(&config, "openai"),
                &Requirement::chat(),
                UnknownPolicy::Invocation,
            )
            .expect("test: selected hint resolves");
            let provider = crate::providers::create_provider_for(
                &resolved,
                &catalog,
                &reliability,
                &crate::providers::ProviderRuntimeOptions::default(),
            )
            .expect("test: exact route provider builds");
            provider
                .chat_with_system(None, "probe", resolved.model(), 0.0)
                .await
                .expect("test: loopback chat");
            assert!(probe.saw(key), "selected route key must reach its provider");
        }
        let requests = probe.requests();
        assert_eq!(requests.len(), 2, "each hint makes exactly one request");
        let first_request = requests.first().expect("first request");
        let second_request = requests.get(1).expect("second request");
        assert!(first_request.carries("PRX-HINT-ROUTE-A"));
        assert!(!first_request.carries("PRX-HINT-ROUTE-B"));
        assert!(second_request.carries("PRX-HINT-ROUTE-B"));
        assert!(!second_request.carries("PRX-HINT-ROUTE-A"));
        assert!(requests.iter().all(|request| !request.carries("PMMC-SELECTION-ROOT")));

        config.model_routes.get_mut(0).expect("first route").credential_ref = Some(foreign.id);
        let wrong_owner = check_default(&config, UnknownPolicy::Invocation).expect_err("foreign owner refused");
        assert!(matches!(wrong_owner, ResolutionError::CredentialOwnerMismatch { .. }));
        config.model_routes.get_mut(0).expect("first route").credential_ref = Some("cred-missing".to_string());
        assert!(check_default(&config, UnknownPolicy::Invocation).is_err());
        assert_eq!(probe.requests().len(), 2, "refusals send nothing");
    }

    /// D-065 L-2: the warning names every surface that routes hints,
    /// `prx agent` included.
    #[test]
    fn the_unroutable_hint_warning_names_every_routing_surface() {
        let warning = warn_if_hint_unroutable("the gateway", "hint:fast").expect("test: a hint warns");
        for surface in ["`prx chat`", "`prx agent`", "`prx go --message`", "cron", "xin"] {
            assert!(warning.contains(surface), "{surface} missing: {warning}");
        }
        assert!(warn_if_hint_unroutable("the gateway", "vendor/model").is_none());
    }

    /// D-035 / R-PMMCf R-3: two routes may not share a hint (the check took
    /// the first, the router the last); a route whose model is itself a hint
    /// and a hint spelled with whitespace are refused where they are checked.
    #[test]
    fn two_routes_may_not_share_a_hint() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let mut duplicate = config_in(&temp, "openai", "gpt-4o");
        duplicate.model_routes = vec![
            route("fast", "openrouter", "vendor/a"),
            route("fast", "openrouter", "vendor/b"),
        ];
        let error = duplicate
            .validate()
            .expect_err("duplicate hints are a configuration error");
        assert!(format!("{error:#}").contains("fast"), "{error:#}");
    }

    #[test]
    fn a_route_whose_model_is_a_hint_is_refused() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let mut nested = config_in(&temp, "openai", "gpt-4o");
        nested.model_routes = vec![route("a", "openrouter", "hint:b"), route("b", "openrouter", "vendor/b")];
        check(
            &nested,
            "openai",
            "hint:a",
            &Requirement::chat(),
            UnknownPolicy::Invocation,
        )
        .expect_err("a route to another hint is refused");
        check_routes(&nested, &catalog_for(&nested), UnknownPolicy::Invocation)
            .expect_err("chat start-up refuses a route whose model is a hint");
    }

    /// R-PMMCg / R-PMMCh H-1: the router looks a hint up exactly while the
    /// checks trim, so a configured hint with whitespace is selected
    /// unreliably. It fails to load instead.
    #[test]
    fn a_configured_hint_with_whitespace_fails_to_load() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        for hint in ["my fast", "fast ", " fast", "fa\tst"] {
            let mut spaced = config_in(&temp, "openai", "gpt-4o");
            spaced.model_routes = vec![route(hint, "openrouter", "vendor/fast")];
            let error = spaced
                .validate()
                .expect_err("a hint with whitespace is a configuration error");
            assert!(format!("{error:#}").contains("whitespace"), "{hint:?}: {error:#}");
        }
    }

    /// An empty `hint:` names no route; the refusal must not blame whitespace.
    #[test]
    fn an_empty_hint_is_refused_without_blaming_whitespace() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let mut config = config_in(&temp, "openai", "gpt-4o");
        config.model_routes = vec![route("fast", "openrouter", "vendor/fast")];
        let error = check(
            &config,
            "openai",
            "hint:",
            &Requirement::chat(),
            UnknownPolicy::Invocation,
        )
        .expect_err("an empty hint names no route");
        let text = error.to_string();
        assert!(!text.contains("whitespace"), "{text}");
        assert!(text.contains("hint:<name>"), "{text}");
    }

    #[test]
    fn a_hint_spelled_with_whitespace_is_refused() {
        let temp = tempfile::TempDir::new().expect("test: temp dir");
        let mut spaced = config_in(&temp, "openai", "gpt-4o");
        spaced.model_routes = vec![route("fast", "openrouter", "vendor/fast")];
        let error = check(
            &spaced,
            "openai",
            "hint: fast",
            &Requirement::chat(),
            UnknownPolicy::Invocation,
        )
        .expect_err("the router would not find ' fast'");
        assert!(error.to_string().contains("whitespace"), "{error}");
    }
}
