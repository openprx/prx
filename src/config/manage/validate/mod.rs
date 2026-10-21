//! Model validation for every selection a configuration change affects
//! (PMM-D-5, D-078).
//!
//! A selection is a place the configuration names a provider and a model:
//!
//! - chat: the default; each `[[model_routes]]` destination; each agent
//!   binding; each `reliability.fallback_providers` member (serving the
//!   default model, each route's model, and each model fallback of a
//!   primary the default or a route serves) and each `reliability.model_fallbacks` entry (on the
//!   provider of the primary it falls back from); each `[[router.models]]`
//!   candidate; the automix premium model and each task-routing
//!   `model_hint`/`sub_agent_model` (a route hint or a model on the default
//!   provider); the smart-group classifier;
//! - embeddings: each `[[embedding_routes]]` destination and the memory
//!   embedder (`memory.embedding_provider`/`embedding_model`, after its
//!   `hint:` embedding route);
//! - audio transcription (`media.audio_model`): PRX's resolver has no
//!   transcription operation, so only its shape is checked (a model is named
//!   for the Ollama transcriber, and it is not a routing hint).
//!
//! A change *affects* a selection when it adds or alters it. It affects
//! every selection when it changes the operator's declarations
//! (`[providers]`), and every selection that names a route (`hint:<x>`) when
//! it changes the routes, since both are evidence the resolver reads. A
//! task-routing hint or the automix premium tier that named a route the
//! change removes is refused too, not read as a raw model (D-080 N-1).
//!
//! Every affected selection is resolved through the one shared check
//! ([`selection::check_model_unproven`]) before anything is written, whatever
//! surface built the change:
//!
//! - an added or altered selection is resolved under the actor's policy
//!   (D-018 Q1, D-034): the operator's surfaces accept a model the evidence
//!   neither proves nor refutes and report it unverified; a model's surfaces
//!   (`config.patch`, `proxy_config`) refuse it;
//! - an unchanged selection re-checked because the evidence changed is
//!   resolved as a running selection: only negative evidence refuses it.
//!
//! Every refused selection is listed, not only the first. The check is local:
//! the declarations in the candidate and the model cache under its
//! workspace. Nothing is sent anywhere and no credential is read.

mod causal;

pub(crate) use causal::{
    check_decision_plane, orphaned_credentials_warning as decision_orphans_warning,
    startup_check as decision_startup_check,
};

use super::history::ConfigActor;
use crate::config::Config;
use crate::providers::catalog::{CatalogView, Requirement, UnknownPolicy};
use crate::providers::selection;

/// The policy a durable selection made by `actor` is resolved under.
#[must_use]
pub(crate) const fn durable_policy(actor: ConfigActor) -> UnknownPolicy {
    match actor {
        ConfigActor::AgentConfigPatch | ConfigActor::AgentProxyConfig => UnknownPolicy::DurableSelection,
        ConfigActor::GatewayConfigApi
        | ConfigActor::GatewayConfigFile
        | ConfigActor::GatewayCredential
        | ConfigActor::Rollback
        | ConfigActor::Cli
        | ConfigActor::GatewayManagement => UnknownPolicy::OperatorSelection,
    }
}

/// What a selection is resolved for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SiteKind {
    Chat,
    Embedding,
    /// Audio transcription: shape only (no resolver operation exists).
    Transcription,
}

/// One selection the configuration makes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectionSite {
    /// How messages name it: `default model`, "model route `hint:fast`", ...
    name: String,
    provider: String,
    model: String,
    kind: SiteKind,
    /// A task-routing hint or the automix premium tier: a route hint when a
    /// route carries it, else a model on the default provider.
    routed: bool,
}

impl SelectionSite {
    fn new(name: impl Into<String>, provider: &str, model: &str, kind: SiteKind) -> Self {
        Self {
            name: name.into(),
            provider: provider.trim().to_string(),
            model: model.trim().to_string(),
            kind,
            routed: false,
        }
    }

    /// A chat selection [`routed_target`] resolved.
    fn routed(name: impl Into<String>, (provider, model): (String, String)) -> Self {
        Self {
            routed: true,
            ..Self::new(name, &provider, &model, SiteKind::Chat)
        }
    }

    /// Whether the selection names a route instead of a model.
    fn names_a_route(&self) -> bool {
        self.model.starts_with("hint:")
    }
}

/// A model a task-routing rule or the automix premium tier names: a route
/// hint when a route carries it (the agent sends it as `hint:<x>`), a model
/// on the default provider otherwise.
fn routed_target(config: &Config, target: &str) -> (String, String) {
    let target = target.trim();
    if config.model_routes.iter().any(|route| route.hint == target) {
        (
            selection::default_provider(config).to_string(),
            format!("hint:{target}"),
        )
    } else {
        (selection::default_provider(config).to_string(), target.to_string())
    }
}

/// Every selection `config` makes.
fn selections(config: &Config) -> Vec<SelectionSite> {
    let default_provider = selection::default_provider(config);
    let default_model = selection::default_model(config);
    let mut sites = vec![SelectionSite::new(
        "default model",
        default_provider,
        default_model,
        SiteKind::Chat,
    )];
    for route in &config.model_routes {
        sites.push(SelectionSite::new(
            format!("model route `hint:{}`", route.hint),
            &route.provider,
            &route.model,
            SiteKind::Chat,
        ));
    }
    for (name, agent) in &config.agents {
        sites.push(SelectionSite::new(
            format!("agent `{name}`"),
            &agent.provider,
            &agent.model,
            SiteKind::Chat,
        ));
    }
    for member in &config.reliability.fallback_providers {
        sites.push(SelectionSite::new(
            format!(
                "fallback provider '{}'",
                super::service::redact_provider_text(member.trim())
            ),
            member,
            default_model,
            SiteKind::Chat,
        ));
        // A route's chain keeps the fallback providers: each is offered the
        // route's own model too (D-083 C-3).
        for route in &config.model_routes {
            sites.push(SelectionSite::new(
                format!(
                    "fallback provider '{}' serving model route `hint:{}`",
                    super::service::redact_provider_text(member.trim()),
                    route.hint
                ),
                member,
                &route.model,
                SiteKind::Chat,
            ));
        }
    }
    let mut primaries = config.reliability.model_fallbacks.iter().collect::<Vec<_>>();
    primaries.sort_by(|left, right| left.0.cmp(right.0));
    for (primary, fallbacks) in primaries {
        // A fallback model serves on the provider of the primary it falls
        // back from: the default, or any route to that model.
        let providers = std::iter::once((default_provider, default_model))
            .chain(
                config
                    .model_routes
                    .iter()
                    .map(|route| (route.provider.as_str(), route.model.as_str())),
            )
            .filter(|(_, model)| model.trim() == primary.trim())
            .map(|(provider, _)| provider)
            .collect::<Vec<_>>();
        let served = !providers.is_empty();
        for provider in providers {
            for fallback in fallbacks.iter().filter(|fallback| !fallback.trim().is_empty()) {
                sites.push(SelectionSite::new(
                    format!("model fallback '{}' of '{}'", fallback.trim(), primary.trim()),
                    provider,
                    fallback,
                    SiteKind::Chat,
                ));
            }
        }
        // The chain offers every fallback provider each model fallback of a
        // primary it serves, as it offers the primary (D-081).
        if served {
            for member in &config.reliability.fallback_providers {
                for fallback in fallbacks.iter().filter(|fallback| !fallback.trim().is_empty()) {
                    sites.push(SelectionSite::new(
                        format!(
                            "fallback provider '{}' serving model fallback '{}' of '{}'",
                            super::service::redact_provider_text(member.trim()),
                            fallback.trim(),
                            primary.trim()
                        ),
                        member,
                        fallback,
                        SiteKind::Chat,
                    ));
                }
            }
        }
    }
    for candidate in &config.router.models {
        // `ollama` with model `*` stands for the default Ollama model.
        if candidate.model_id.trim() == "*" {
            continue;
        }
        sites.push(SelectionSite::new(
            format!("router candidate '{}'", candidate.model_id.trim()),
            &candidate.provider,
            &candidate.model_id,
            SiteKind::Chat,
        ));
    }
    let premium = config.router.automix.premium_model_id.trim();
    if !premium.is_empty() {
        sites.push(SelectionSite::routed(
            "automix premium model",
            routed_target(config, premium),
        ));
    }
    for (index, rule) in config.task_routing.rules.iter().enumerate() {
        if let Some(hint) = rule.model_hint.as_deref().filter(|hint| !hint.trim().is_empty()) {
            sites.push(SelectionSite::routed(
                format!("task routing rule {index} model_hint"),
                routed_target(config, hint),
            ));
        }
        if let Some(model) = rule.sub_agent_model.as_deref().filter(|model| !model.trim().is_empty()) {
            sites.push(SelectionSite::new(
                format!("task routing rule {index} sub_agent_model"),
                default_provider,
                model,
                SiteKind::Chat,
            ));
        }
    }
    let classifier = &config.smart_group;
    if !classifier.classifier_provider.trim().is_empty() {
        let model = if classifier.classifier_model.trim().is_empty() {
            default_model
        } else {
            classifier.classifier_model.as_str()
        };
        sites.push(SelectionSite::new(
            "smart-group classifier",
            &classifier.classifier_provider,
            model,
            SiteKind::Chat,
        ));
    }
    for route in &config.embedding_routes {
        sites.push(SelectionSite::new(
            format!("embedding route `hint:{}`", route.hint),
            &route.provider,
            &route.model,
            SiteKind::Embedding,
        ));
    }
    let (embedder_provider, embedder_model) = crate::memory::configured_embedder(config);
    sites.push(SelectionSite::new(
        "memory embedder",
        &embedder_provider,
        &embedder_model,
        SiteKind::Embedding,
    ));
    sites.push(SelectionSite::new(
        "audio transcription model",
        &config.media.audio_provider,
        &config.media.audio_model,
        SiteKind::Transcription,
    ));
    sites
}

/// Every selection `config` makes, as `(name, provider)`: the name the
/// validator's messages give it (`default model`, "model route
/// `hint:fast`", "agent `coder`", ...) and the provider it uses. This is the
/// list a provider's uses and a removal's referrers are taken from: exactly
/// the sites the validator resolves.
pub(crate) fn provider_uses(config: &Config) -> Vec<(String, String)> {
    selections(config)
        .into_iter()
        .filter(|site| !site.provider.is_empty())
        .map(|site| (site.name, site.provider))
        .collect()
}

/// The operations the selections of `config` use `model` of `provider` for
/// (chat, embedding), each once: what a declaration of that model must keep
/// (D-102 H-1). Providers compare by identity, models ignoring ASCII case.
pub(crate) fn operations_selected_for(
    config: &Config,
    provider: &str,
    model: &str,
) -> Vec<crate::providers::catalog::ModelOperation> {
    use crate::providers::catalog::ModelOperation;
    let identity = crate::providers::ProviderId::of(provider);
    let mut used = Vec::new();
    for site in selections(config) {
        let operation = match site.kind {
            SiteKind::Chat => ModelOperation::Chat,
            SiteKind::Embedding => ModelOperation::Embedding,
            SiteKind::Transcription => continue,
        };
        if crate::providers::ProviderId::of(&site.provider) == identity
            && site.model.eq_ignore_ascii_case(model.trim())
            && !used.contains(&operation)
        {
            used.push(operation);
        }
    }
    used
}

/// Every `(provider, model)` a chat or embedding selection of `config`
/// names directly (a `hint:` route name and PRX's own embedders are left
/// out): the configured models a model listing starts from.
pub(crate) fn selected_models(config: &Config) -> Vec<(String, String)> {
    selections(config)
        .into_iter()
        .filter(|site| {
            site.kind != SiteKind::Transcription
                && !site.names_a_route()
                && !site.model.is_empty()
                && !selection::is_prx_embedder(&site.provider)
        })
        .map(|site| (site.provider, site.model))
        .collect()
}

/// Every `credential_ref` of `config` as `(name, provider, reference id)`,
/// each named as the validator names the selection (a model route, an
/// embedding route or an agent).
pub(crate) fn credential_reference_holders(config: &Config) -> Vec<(String, String, String)> {
    let reference = |id: Option<&String>| id.map(|id| id.trim().to_string()).filter(|id| !id.is_empty());
    let mut holders = Vec::new();
    for route in &config.model_routes {
        if let Some(id) = reference(route.credential_ref.as_ref()) {
            holders.push((
                format!("model route `hint:{}`", route.hint),
                route.provider.trim().to_string(),
                id,
            ));
        }
    }
    for route in &config.embedding_routes {
        if let Some(id) = reference(route.credential_ref.as_ref()) {
            holders.push((
                format!("embedding route `hint:{}`", route.hint),
                route.provider.trim().to_string(),
                id,
            ));
        }
    }
    let mut agents = config.agents.iter().collect::<Vec<_>>();
    agents.sort_by(|left, right| left.0.cmp(right.0));
    for (name, agent) in agents {
        if let Some(id) = reference(agent.credential_ref.as_ref()) {
            holders.push((format!("agent `{name}`"), agent.provider.trim().to_string(), id));
        }
    }
    holders
}

/// Every `credential_ref` `candidate` adds or re-targets (compared with
/// `current`; every one when there is none) must name a credential
/// reference that exists in the store beside the candidate's `config.toml`
/// and was issued for the provider of its route, embedding route or agent:
/// the owner binding of SP-PMM §2.4. A rollback that would restore a
/// reference removed since is refused here too, with nothing written.
///
/// # Errors
///
/// Every refused reference, each as one sentence naming the selection and
/// the reference id (never a value), joined.
pub(crate) fn check_credential_references(current: Option<&Config>, candidate: &Config) -> Result<(), String> {
    let known = current.map(credential_reference_holders).unwrap_or_default();
    let added = credential_reference_holders(candidate)
        .into_iter()
        .filter(|holder| !known.contains(holder))
        .collect::<Vec<_>>();
    if added.is_empty() {
        return Ok(());
    }
    let references =
        super::credentials::CredentialReferences::for_config(candidate).map_err(|error| error.to_string())?;
    let refusals = added
        .into_iter()
        .filter_map(|(name, provider, id)| {
            references
                .check_reference(&id, &provider)
                .err()
                .map(|error| format!("{name} refused: {error}"))
        })
        .collect::<Vec<_>>();
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(refusals.join("; "))
    }
}

/// Whether the two renderings differ; a rendering failure counts as a
/// change (everything is then re-checked, which can only refuse more).
pub(super) fn rendering_changed<T: serde::Serialize>(current: &T, candidate: &T) -> bool {
    match (serde_json::to_value(current), serde_json::to_value(candidate)) {
        (Ok(current), Ok(candidate)) => current != candidate,
        _ => true,
    }
}

/// Resolve every selection `candidate` makes that the change from `current`
/// affects (`None`: nothing is stored yet, so every selection is new).
///
/// Returns one warning per selection accepted as unverified.
///
/// # Errors
///
/// Every refused selection, each as one sentence that names it and gives
/// the resolver's reason, joined. It carries no credential.
pub(crate) fn check_affected_selections(
    current: Option<&Config>,
    candidate: &Config,
    policy: UnknownPolicy,
) -> Result<Vec<String>, String> {
    if policy == UnknownPolicy::Invocation {
        crate::config::model_settings::validate_existing(candidate)?;
    } else {
        crate::config::model_settings::validate_changes(current, candidate)?;
    }
    let current_sites = current.map(selections).unwrap_or_default();
    let declarations_changed =
        current.is_none_or(|current| rendering_changed(&current.providers, &candidate.providers));
    let routes_changed = current.is_none_or(|current| {
        rendering_changed(&current.model_routes, &candidate.model_routes)
            || rendering_changed(&current.embedding_routes, &candidate.embedding_routes)
    });
    let catalog = selection::catalog_for(candidate);
    let mut warnings = Vec::new();
    let mut refusals = Vec::new();
    for site in selections(candidate) {
        if let Some(refusal) = lost_route(&current_sites, &site) {
            refusals.push(refusal);
            continue;
        }
        let site_policy = if current.is_none() || !current_sites.contains(&site) {
            policy
        } else if declarations_changed || (routes_changed && site.names_a_route()) {
            UnknownPolicy::Invocation
        } else {
            continue;
        };
        match check_site(&catalog, &site, site_policy) {
            // A running selection's unverified state is not news; only a
            // new durable selection's is reported.
            Ok(Some(warning)) if site_policy != UnknownPolicy::Invocation => warnings.push(warning),
            Ok(_) => {}
            Err(refusal) => refusals.push(refusal),
        }
    }
    if current.is_none_or(|current| {
        selection::default_provider(current) != selection::default_provider(candidate)
            || selection::default_model(current) != selection::default_model(candidate)
    }) && let Ok(resolved) = selection::check(
        candidate,
        selection::default_provider(candidate),
        selection::default_model(candidate),
        &Requirement::chat(),
        UnknownPolicy::Invocation,
    ) && let Some(warning) = selection::pricing_warning(&resolved, &candidate.cost)
    {
        warnings.push(warning);
    }
    // The decision backend: the same check a reload runs (CDP-12).
    match check_decision_plane(current, candidate) {
        Ok(Some(warning)) => warnings.push(warning),
        Ok(None) => {}
        Err(refusal) => refusals.push(refusal),
    }
    if refusals.is_empty() {
        Ok(warnings)
    } else {
        Err(refusals.join("; "))
    }
}

/// Inspect persisted selections as invocations, so lifecycle changes never
/// turn a diagnostic into a request to make a new durable selection.
pub(crate) fn check_existing_selections(config: &Config) -> Result<Vec<String>, String> {
    crate::config::model_settings::validate_existing(config)?;
    let catalog = selection::catalog_for(config);
    let mut warnings = Vec::new();
    let mut refused = Vec::new();
    for site in selections(config) {
        match check_site(&catalog, &site, UnknownPolicy::Invocation) {
            Ok(Some(warning)) => warnings.push(warning),
            Ok(None) => {}
            Err(error) => refused.push(error),
        }
    }
    if refused.is_empty() {
        Ok(warnings)
    } else {
        Err(refused.join("; "))
    }
}

/// A routed selection that named a route before the change and, the route
/// gone, would quietly become a raw model on the default provider: refused
/// like every other selection that names a removed route (D-080 N-1).
fn lost_route(current_sites: &[SelectionSite], site: &SelectionSite) -> Option<String> {
    if !site.routed || site.names_a_route() {
        return None;
    }
    let route = format!("hint:{}", site.model);
    current_sites
        .iter()
        .any(|before| before.routed && before.name == site.name && before.model == route)
        .then(|| {
            format!(
                "{} refused: it names route `{route}`, which this change removes; keep the route or change the \
                 selection",
                site.name
            )
        })
}

/// Resolve one selection. `Ok(Some(warning))` when it is accepted as
/// unverified.
fn check_site(catalog: &CatalogView, site: &SelectionSite, policy: UnknownPolicy) -> Result<Option<String>, String> {
    let refused = |reason: &dyn std::fmt::Display| {
        let mut message = format!("{} refused: {reason}", site.name);
        if site.name
            == format!(
                "fallback provider '{}'",
                super::service::redact_provider_text(&site.provider)
            )
        {
            message.push_str("; provider fallbacks inherit the primary model. Configure alternate model IDs in [reliability.model_fallbacks]; every configured provider/model pair must still be compatible (see docs/providers.md)");
        }
        message
    };
    let unverified = match site.kind {
        SiteKind::Chat => {
            selection::check_model_unproven(catalog, &site.provider, &site.model, &Requirement::chat(), policy)
        }
        SiteKind::Embedding => {
            // The memory embedder follows embedding routes; an embedding hint
            // no route carries is left unresolved here.
            if site.names_a_route() && !selection::is_prx_embedder(&site.provider) {
                return Err(refused(&format_args!(
                    "it names embedding route `{}`, which no `[[embedding_routes]]` entry carries",
                    site.model
                )));
            }
            selection::check_embedder_unproven(catalog, &site.provider, &site.model, policy)
        }
        SiteKind::Transcription => {
            return check_transcription(site)
                .map(|()| None)
                .map_err(|reason| refused(&reason));
        }
    }
    .map_err(|error| refused(&error))?;
    Ok(unverified.map(|unverified| {
        format!(
            "{} is unverified for {}: PRX has no catalog evidence that model '{}' supports it",
            site.name,
            unverified.unproven.join(", "),
            unverified.model
        )
    }))
}

/// Audio transcription has no resolver operation; check what can be
/// checked: the Ollama transcriber needs a model, and a model is never a
/// routing hint (media does not route).
fn check_transcription(site: &SelectionSite) -> Result<(), &'static str> {
    if site.provider.eq_ignore_ascii_case("ollama") && site.model.is_empty() {
        return Err("the Ollama transcriber needs `media.audio_model`");
    }
    if site.names_a_route() {
        return Err("`media.audio_model` is a model name, not a routing hint");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declare(config: &mut Config, provider: &str, model: &str, operations: &str) {
        let parsed: crate::config::schema::ProviderModelConfig = toml::from_str(&format!(
            "provider = \"{provider}\"\nmodel = \"{model}\"\noperations = {operations}\n"
        ))
        .expect("test: a valid [[providers.models]] entry");
        config.providers.models.push(parsed);
    }

    const ENDPOINT: &str = "custom:http://127.0.0.1:9/v1";

    fn isolated() -> (tempfile::TempDir, Config) {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = Config {
            config_path: dir.path().join("config.toml"),
            workspace_dir: dir.path().join("workspace"),
            ..Config::default()
        };
        (dir, config)
    }

    #[test]
    fn an_unchanged_configuration_checks_nothing() {
        let (_dir, mut config) = isolated();
        config.model_routes.push(crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "x".into(),
            provider: "openrouter".into(),
            model: "vendor/never-seen".into(),
            api_key: None,
            credential_ref: None,
        });
        assert_eq!(
            check_affected_selections(Some(&config), &config, UnknownPolicy::DurableSelection),
            Ok(Vec::new())
        );
    }

    #[test]
    fn the_operator_gets_a_warning_and_a_model_actor_a_refusal_for_an_unverified_route() {
        let (_dir, current) = isolated();
        let mut candidate = current.clone();
        candidate.model_routes.push(crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: "x".into(),
            provider: "openrouter".into(),
            model: "vendor/never-seen".into(),
            api_key: None,
            credential_ref: None,
        });

        let warnings = check_affected_selections(Some(&current), &candidate, UnknownPolicy::OperatorSelection)
            .expect("test: the operator may select an unverified model");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings
                .first()
                .is_some_and(|warning| warning.contains("hint:x") && warning.contains("unverified")),
            "{warnings:?}"
        );

        let refusal = check_affected_selections(Some(&current), &candidate, UnknownPolicy::DurableSelection)
            .expect_err("test: a model may not");
        assert!(refusal.contains("model route `hint:x` refused"), "{refusal}");
    }

    #[test]
    fn an_embedding_route_to_a_chat_only_model_is_refused() {
        let (_dir, current) = isolated();
        let mut candidate = current.clone();
        declare(&mut candidate, ENDPOINT, "chat-only", "[\"chat\"]");
        candidate.embedding_routes.push(crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: ENDPOINT.into(),
            model: "chat-only".into(),
            dimensions: None,
            api_key: None,
            credential_ref: None,
        });
        let refusal = check_affected_selections(Some(&current), &candidate, UnknownPolicy::OperatorSelection)
            .expect_err("test: refused");
        assert!(refusal.contains("embedding route `hint:semantic` refused"), "{refusal}");
    }

    #[test]
    fn a_first_write_checks_every_selection_and_prs_own_embedders_always_resolve() {
        let (_dir, mut candidate) = isolated();
        candidate.embedding_routes.push(crate::config::EmbeddingRouteConfig {
            hint: "kw".into(),
            provider: "none".into(),
            model: "none".into(),
            dimensions: None,
            api_key: None,
            credential_ref: None,
        });
        declare(&mut candidate, ENDPOINT, "embed-only", "[\"embedding\"]");
        candidate.default_provider = Some(ENDPOINT.into());
        candidate.default_model = Some("embed-only".into());
        let refusal =
            check_affected_selections(None, &candidate, UnknownPolicy::OperatorSelection).expect_err("test: refused");
        assert!(refusal.starts_with("default model refused"), "{refusal}");
    }

    #[test]
    fn model_actors_are_strict_and_operators_are_not() {
        assert_eq!(
            durable_policy(ConfigActor::AgentConfigPatch),
            UnknownPolicy::DurableSelection
        );
        assert_eq!(
            durable_policy(ConfigActor::AgentProxyConfig),
            UnknownPolicy::DurableSelection
        );
        for actor in [
            ConfigActor::GatewayConfigApi,
            ConfigActor::GatewayConfigFile,
            ConfigActor::GatewayCredential,
            ConfigActor::Rollback,
            ConfigActor::Cli,
            ConfigActor::GatewayManagement,
        ] {
            assert_eq!(durable_policy(actor), UnknownPolicy::OperatorSelection);
        }
    }

    fn route(hint: &str, provider: &str, model: &str) -> crate::config::ModelRouteConfig {
        crate::config::ModelRouteConfig {
            settings: Default::default(),
            hint: hint.into(),
            provider: provider.into(),
            model: model.into(),
            api_key: None,
            credential_ref: None,
        }
    }

    /// RED before PMMDtail-f (D-078 T-2): removing a route left every
    /// selection that names it (the default, an agent) unchecked. They are
    /// all refused now, each named.
    #[test]
    fn removing_a_route_refuses_every_selection_that_names_it() {
        let (_dir, mut current) = isolated();
        current.model_routes.push(route("fast", "openrouter", "vendor/model-a"));
        current.default_model = Some("hint:fast".into());
        let agent: crate::config::DelegateAgentConfig =
            serde_json::from_value(serde_json::json!({"provider": "openrouter", "model": "hint:fast"}))
                .expect("test: agent");
        current.agents.insert("helper".into(), agent);
        let mut candidate = current.clone();
        candidate.model_routes.clear();

        let refusal = check_affected_selections(Some(&current), &candidate, UnknownPolicy::OperatorSelection)
            .expect_err("test: dependents of a removed route are refused");

        assert!(refusal.contains("default model refused"), "{refusal}");
        assert!(refusal.contains("agent `helper` refused"), "{refusal}");

        // Renaming the route is the same change.
        let mut renamed = current.clone();
        renamed.model_routes = vec![route("quick", "openrouter", "vendor/model-a")];
        assert!(check_affected_selections(Some(&current), &renamed, UnknownPolicy::OperatorSelection).is_err());
    }

    /// RED before PMMDtail-f (D-078 T-3): the memory embedder is the
    /// embedding default; a model actor's unverified choice is refused, and a
    /// chat-only model is refused for everyone.
    #[test]
    fn the_memory_embedder_is_a_selection() {
        let (_dir, current) = isolated();
        let mut candidate = current.clone();
        candidate.memory.embedding_provider = "openrouter".into();
        candidate.memory.embedding_model = "vendor/never-seen".into();
        let refusal = check_affected_selections(Some(&current), &candidate, UnknownPolicy::DurableSelection)
            .expect_err("test: strict for a model");
        assert!(refusal.contains("memory embedder refused"), "{refusal}");

        let mut chat_only = current.clone();
        declare(&mut chat_only, ENDPOINT, "chat-only", "[\"chat\"]");
        chat_only.memory.embedding_provider = ENDPOINT.into();
        chat_only.memory.embedding_model = "chat-only".into();
        assert!(check_affected_selections(Some(&current), &chat_only, UnknownPolicy::OperatorSelection).is_err());

        let mut dangling = current.clone();
        dangling.memory.embedding_provider = "openai".into();
        dangling.memory.embedding_model = "hint:missing".into();
        let refusal = check_affected_selections(Some(&current), &dangling, UnknownPolicy::OperatorSelection)
            .expect_err("test: an unknown embedding hint");
        assert!(refusal.contains("hint:missing"), "{refusal}");
    }

    /// RED before PMMDtail-f (D-078 T-3): fallbacks, router candidates, the
    /// automix premium model, task routing and the smart-group classifier
    /// are selections too.
    #[test]
    fn every_selection_bearing_field_is_checked() {
        let (_dir, mut current) = isolated();
        declare(&mut current, ENDPOINT, "embed-only", "[\"embedding\"]");
        let changes: Vec<(&str, Box<dyn Fn(&mut Config)>)> = vec![
            (
                "fallback provider",
                Box::new(|config: &mut Config| {
                    config.default_provider = Some("openrouter".into());
                    config.default_model = Some("embed-only".into());
                    config.reliability.fallback_providers = vec![ENDPOINT.into()];
                }),
            ),
            (
                "model fallback",
                Box::new(|config: &mut Config| {
                    config.default_provider = Some(ENDPOINT.into());
                    config.default_model = Some("chat-model".into());
                    config
                        .reliability
                        .model_fallbacks
                        .insert("chat-model".into(), vec!["embed-only".into()]);
                }),
            ),
            (
                "router candidate",
                Box::new(|config: &mut Config| {
                    config.router.models.push(
                        serde_json::from_value(serde_json::json!({
                            "model_id": "embed-only",
                            "provider": ENDPOINT,
                        }))
                        .expect("test: router model"),
                    );
                }),
            ),
            (
                "automix premium model",
                Box::new(|config: &mut Config| {
                    config.default_provider = Some(ENDPOINT.into());
                    config.router.automix.premium_model_id = "embed-only".into();
                }),
            ),
            (
                "sub_agent_model",
                Box::new(|config: &mut Config| {
                    config.default_provider = Some(ENDPOINT.into());
                    config.task_routing.rules.push(
                        serde_json::from_value(serde_json::json!({
                            "keywords": ["x"],
                            "intent": "delegate",
                            "sub_agent_model": "embed-only",
                        }))
                        .expect("test: task routing rule"),
                    );
                }),
            ),
            (
                "smart-group classifier",
                Box::new(|config: &mut Config| {
                    config.smart_group.classifier_provider = ENDPOINT.into();
                    config.smart_group.classifier_model = "embed-only".into();
                }),
            ),
        ];
        for (name, change) in changes {
            let mut candidate = current.clone();
            change(&mut candidate);
            let refusal = check_affected_selections(Some(&current), &candidate, UnknownPolicy::OperatorSelection)
                .expect_err(name);
            assert!(refusal.contains(name), "{name}: {refusal}");
        }
    }

    /// RED before PMMDtail-g (D-080 N-1): removing the route a task-routing
    /// `model_hint` (or the automix premium tier) names turned the hint into
    /// a raw model on the default provider, accepted as unverified. It is
    /// refused like every other selection that names the route.
    #[test]
    fn removing_a_route_a_task_rule_names_is_refused() {
        let (_dir, mut current) = isolated();
        current.model_routes.push(route("trx", "openrouter", "vendor/model-a"));
        current.task_routing.rules.push(
            serde_json::from_value(serde_json::json!({"keywords": ["x"], "model_hint": "trx"}))
                .expect("test: task routing rule"),
        );
        current.router.automix.premium_model_id = "trx".into();
        let mut candidate = current.clone();
        candidate.model_routes.clear();

        let refusal = check_affected_selections(Some(&current), &candidate, UnknownPolicy::OperatorSelection)
            .expect_err("test: the rule's route was removed");

        assert!(refusal.contains("task routing rule 0 model_hint refused"), "{refusal}");
        assert!(refusal.contains("automix premium model refused"), "{refusal}");
        assert!(refusal.contains("hint:trx"), "{refusal}");

        // A rule that names a model, not a route, is not affected.
        let mut raw = current.clone();
        raw.model_routes.clear();
        raw.task_routing.rules.clear();
        raw.router.automix.premium_model_id = String::new();
        raw.default_provider = Some("openrouter".into());
        let mut raw_candidate = raw.clone();
        raw_candidate.task_routing.rules.push(
            serde_json::from_value(serde_json::json!({"keywords": ["x"], "model_hint": "vendor/model-a"}))
                .expect("test: task routing rule"),
        );
        assert!(
            check_affected_selections(Some(&raw), &raw_candidate, UnknownPolicy::OperatorSelection).is_ok(),
            "a raw model hint is a model, not a lost route"
        );
    }

    /// RED before PMMDtail-h (D-081 (3)): the runtime offers every
    /// `fallback_providers` member the default model and each of its model
    /// fallbacks, but only the default model was checked against the members.
    #[test]
    fn a_fallback_provider_is_checked_against_every_model_fallback() {
        let (_dir, mut current) = isolated();
        current.default_provider = Some("openrouter".into());
        current.default_model = Some("anthropic/claude-sonnet-4".into());
        current.reliability.fallback_providers = vec!["anthropic".into()];
        let mut candidate = current.clone();
        candidate
            .reliability
            .model_fallbacks
            .insert("anthropic/claude-sonnet-4".into(), vec!["openai/gpt-4o".into()]);

        let refusal = check_affected_selections(Some(&current), &candidate, UnknownPolicy::OperatorSelection)
            .expect_err("test: the fallback provider cannot serve the model fallback");

        assert!(
            refusal.contains("fallback provider 'anthropic' serving model fallback 'openai/gpt-4o'"),
            "{refusal}"
        );
    }

    #[test]
    fn model_fallback_hint_is_only_for_the_plain_fallback_provider_site() {
        let (_dir, config) = isolated();
        let catalog = CatalogView::from_config(&config);
        for (name, expected_hint) in [
            ("fallback provider 'kimi-code'", true),
            ("fallback provider 'kimi-code' serving model route `hint:code`", false),
            (
                "fallback provider 'kimi-code' serving model fallback 'anthropic/claude-sonnet-5' of 'kimi-for-coding'",
                false,
            ),
        ] {
            let site = SelectionSite::new(name, "kimi-code", "anthropic/claude-sonnet-5", SiteKind::Chat);
            let refusal = check_site(&catalog, &site, UnknownPolicy::OperatorSelection).unwrap_err();
            assert!(refusal.contains("not compatible"), "{refusal}");
            assert_eq!(
                refusal.contains("[reliability.model_fallbacks]"),
                expected_hint,
                "{refusal}"
            );
        }
    }

    /// RED before PMMDtail-i (D-083 C-3): a route's chain keeps the
    /// `fallback_providers`, so each member is offered the route's own model;
    /// only the default model and model fallbacks were checked against them.
    #[test]
    fn a_fallback_provider_is_checked_against_every_route_model() {
        let (_dir, mut current) = isolated();
        current.default_provider = Some("openrouter".into());
        current.default_model = Some("anthropic/claude-sonnet-4".into());
        current.reliability.fallback_providers = vec!["anthropic".into()];
        let mut candidate = current.clone();
        candidate
            .model_routes
            .push(route("code", "openrouter", "openai/gpt-4o"));

        let refusal = check_affected_selections(Some(&current), &candidate, UnknownPolicy::OperatorSelection)
            .expect_err("test: the fallback provider cannot serve the route's model");

        assert!(
            refusal.contains("fallback provider 'anthropic' serving model route `hint:code`"),
            "{refusal}"
        );
    }
}
