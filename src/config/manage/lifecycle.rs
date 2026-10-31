//! Release-only model lifecycle diagnostics and transactional replacement.
use crate::config::Config;
use crate::providers::ProviderId;
use crate::providers::catalog::{ModelLifecycleStatus, ReleaseCatalog};

/// Lifecycle messages for command output, including informational absences.
pub fn warnings(config: &Config) -> anyhow::Result<Vec<String>> {
    Ok(diagnostics(config)?.into_iter().map(|note| note.message).collect())
}

pub(crate) struct LifecycleDiagnostic {
    pub message: String,
    pub warning: bool,
}

/// Unknown catalog selections are informational; open-ended providers do not
/// promise a closed release inventory.
pub(crate) fn diagnostics(config: &Config) -> anyhow::Result<Vec<LifecycleDiagnostic>> {
    let release = ReleaseCatalog::embedded_ref()?;
    let mut selected = super::validate::selected_models(config);
    for id in config.model_settings.keys() {
        if let Some((provider, _)) = id.split_once('/') {
            selected.push((provider.to_owned(), id.clone()));
        }
    }
    selected.sort();
    selected.dedup();
    let mut warnings = Vec::new();
    for (provider, model) in selected {
        let identity = ProviderId::of(&provider);
        if identity.catalog_name().is_none() || model.starts_with("hint:") {
            continue;
        }
        let row = release.model(&identity, &model);
        if row.is_none()
            && release
                .models
                .iter()
                .any(|row| Some(row.provider.as_str()) == identity.catalog_name() && row.api_model == "*")
        {
            continue;
        }
        let state = match row {
            Some(row) if row.lifecycle.status == ModelLifecycleStatus::Active => continue,
            Some(row) => {
                let status = match row.lifecycle.status {
                    ModelLifecycleStatus::Deprecated => "deprecated",
                    ModelLifecycleStatus::Retired => "retired",
                    ModelLifecycleStatus::Active => continue,
                };
                row.lifecycle.replacement.as_deref().map_or_else(
                    || status.to_owned(),
                    |replacement| format!("{status}; published replacement: {replacement}"),
                )
            }
            None => "absent from this release (deleted or custom); unverified without local evidence".into(),
        };
        warnings.push(LifecycleDiagnostic { warning: row.is_some(), message: format!(
            "model '{}' of provider '{}' is {state}; existing configuration is retained; inspect `prx models migrate-retired`",
            crate::security::redaction::redact_provider_text(&model),
            crate::security::redaction::redact_provider_text(&provider),
        ) });
    }
    Ok(warnings)
}

fn replacement(release: &ReleaseCatalog, provider: &str, model: &str) -> anyhow::Result<Option<String>> {
    let identity = ProviderId::of(provider);
    let Some(row) = release.model(&identity, model) else {
        return Ok(None);
    };
    if row.lifecycle.status == ModelLifecycleStatus::Active {
        return Ok(None);
    }
    let Some(target) = row.lifecycle.replacement.as_deref() else {
        return Ok(None);
    };
    // A variant can carry a distinct tariff (for example `:free`). A base
    // replacement cannot authorize silently losing that suffix.
    let spellings = || {
        std::iter::once(row.api_model.as_str())
            .chain(std::iter::once(row.id.as_str()))
            .chain(row.aliases.iter().map(String::as_str))
    };
    let has_variant = !spellings().any(|name| name.eq_ignore_ascii_case(model))
        && spellings().any(|name| {
            model
                .get(..name.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(name))
                && model.get(name.len()..).is_some_and(|suffix| suffix.starts_with(':'))
        });
    anyhow::ensure!(
        !has_variant,
        "model variant cannot be migrated to a base replacement; select its variant explicitly"
    );
    let target = release
        .models
        .iter()
        .find(|candidate| candidate.id == target)
        .ok_or_else(|| anyhow::anyhow!("published model replacement is missing"))?;
    anyhow::ensure!(
        target.is_active(),
        "published replacement must be active; choose a replacement explicitly"
    );
    anyhow::ensure!(
        ProviderId::of(&row.provider) == ProviderId::of(&target.provider),
        "replacement changes provider ownership; select its provider and credential explicitly"
    );
    Ok(Some(target.api_model.clone()))
}

fn replace(release: &ReleaseCatalog, provider: &str, model: &mut String) -> anyhow::Result<()> {
    if let Some(target) = replacement(release, provider, model)? {
        *model = target;
    }
    Ok(())
}

/// Build one atomic PMM candidate. Never change provider or credential ownership.
/// Missing replacement metadata leaves a reference intact for operator inspection.
pub(super) fn candidate(config: &Config) -> anyhow::Result<Config> {
    let release = ReleaseCatalog::embedded_ref()?;
    let mut next = config.clone();
    let provider = crate::providers::selection::default_provider(config);
    if let Some(model) = &mut next.default_model {
        replace(release, provider, model)?;
    }
    for route in &mut next.model_routes {
        replace(release, &route.provider, &mut route.model)?;
    }
    for route in &mut next.embedding_routes {
        replace(release, &route.provider, &mut route.model)?;
    }
    for agent in next.agents.values_mut() {
        replace(release, &agent.provider, &mut agent.model)?;
    }
    replace(
        release,
        &next.memory.embedding_provider,
        &mut next.memory.embedding_model,
    )?;
    next.reliability.model_fallbacks = migrate_fallbacks(config, release)?;
    for row in &mut next.router.models {
        replace(release, &row.provider, &mut row.model_id)?;
    }
    let is_hint =
        |model: &str| model.starts_with("hint:") || config.model_routes.iter().any(|route| route.hint == model);
    if !is_hint(&next.router.automix.premium_model_id) {
        replace(release, provider, &mut next.router.automix.premium_model_id)?;
    }
    for rule in &mut next.task_routing.rules {
        if let Some(model) = &mut rule.model_hint
            && !is_hint(model)
        {
            replace(release, provider, model)?;
        }
        if let Some(model) = &mut rule.sub_agent_model {
            replace(release, provider, model)?;
        }
    }
    if !next.smart_group.classifier_model.is_empty() {
        replace(
            release,
            &next.smart_group.classifier_provider,
            &mut next.smart_group.classifier_model,
        )?;
    }
    for (id, settings) in &config.model_settings {
        let Some(row) = release.models.iter().find(|row| row.id == *id) else {
            continue;
        };
        if let Some(target) = replacement(release, &row.provider, id)? {
            let target = release
                .model(&ProviderId::of(&row.provider), &target)
                .ok_or_else(|| anyhow::anyhow!("replacement model is missing"))?;
            settings.validate(&target.id)?;
            anyhow::ensure!(
                !next.model_settings.contains_key(&target.id),
                "replacement already has model settings; resolve the conflict explicitly"
            );
            next.model_settings.remove(id);
            next.model_settings.insert(target.id.clone(), settings.clone());
        }
    }
    Ok(next)
}

fn migrate_fallbacks(
    config: &Config,
    release: &ReleaseCatalog,
) -> anyhow::Result<std::collections::HashMap<String, Vec<String>>> {
    let provider = crate::providers::selection::default_provider(config);
    let mut fallbacks = std::collections::HashMap::new();
    for (primary, members) in &config.reliability.model_fallbacks {
        let mut owners = Vec::new();
        if crate::providers::selection::default_model(config).trim() == primary.trim() {
            owners.push(provider);
        }
        owners.extend(
            config
                .model_routes
                .iter()
                .filter(|route| route.model.trim() == primary.trim())
                .map(|route| route.provider.as_str()),
        );
        if !owners.is_empty() {
            owners.extend(config.reliability.fallback_providers.iter().map(String::as_str));
        }
        let mut changes = Vec::new();
        for owner in owners {
            let mut key = primary.clone();
            replace(release, owner, &mut key)?;
            let mut values = members.clone();
            for model in &mut values {
                replace(release, owner, model)?;
            }
            changes.push((key, values));
        }
        let changed = changes
            .iter()
            .filter(|pair| pair.0 != *primary || pair.1 != *members)
            .collect::<Vec<_>>();
        if let Some(first) = changed.first() {
            anyhow::ensure!(
                changes.iter().all(|pair| pair == *first),
                "shared fallback models require different replacements; edit those selections explicitly"
            );
        }
        let (key, values) = changes
            .into_iter()
            .next()
            .unwrap_or_else(|| (primary.clone(), members.clone()));
        if key != *primary || values != *members {
            let mut seen = std::collections::BTreeSet::from([key.as_str()]);
            anyhow::ensure!(
                values.iter().all(|model| seen.insert(model.as_str())),
                "replacement collapses fallback models; choose distinct fallbacks explicitly"
            );
        }
        anyhow::ensure!(
            fallbacks.insert(key, values).is_none(),
            "replacement would merge fallback entries"
        );
    }
    Ok(fallbacks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_absence_is_informational_and_open_ended_providers_are_skipped() {
        for (provider, model, absent) in [
            ("custom:https://example.com/v1", "prx-custom-model", false),
            ("ollama", "prx-local-tag:latest", false),
            ("openrouter", "prx/unlisted-model", true),
            ("openai", "prx-unlisted-model", true),
        ] {
            let config = Config {
                default_provider: Some(provider.into()),
                default_model: Some(model.into()),
                ..Default::default()
            };
            let notes = diagnostics(&config).unwrap();
            let selected = notes
                .iter()
                .filter(|note| note.message.contains(model))
                .collect::<Vec<_>>();
            assert_eq!(!selected.is_empty(), absent, "{provider}");
            assert!(selected.iter().all(|note| !note.warning));
        }
        let config = Config {
            default_provider: Some("openai".into()),
            default_model: Some("gpt-4o".into()),
            ..Default::default()
        };
        assert!(
            diagnostics(&config)
                .unwrap()
                .iter()
                .any(|note| note.warning && note.message.contains("retired"))
        );
    }

    #[tokio::test]
    async fn retired_free_variant_refuses_the_real_migration_plan_without_changes() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut config = Config {
            config_path: dir.path().join("config.toml"),
            workspace_dir: dir.path().join("workspace"),
            default_provider: Some("openrouter".into()),
            default_model: Some("moonshotai/kimi-k2.5:free".into()),
            ..Default::default()
        };
        config.save().await.unwrap();
        let before = std::fs::read(&config.config_path).unwrap();
        let service = super::super::ManagementService::deferred(crate::config::new_shared(config.clone()));
        let error = service
            .plan(
                &super::super::ManagedChange::MigrateRetired,
                super::super::ConfigActor::Cli,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("variant"), "{error}");
        assert_eq!(std::fs::read(&config.config_path).unwrap(), before);
        assert_eq!(
            service.config().default_model.as_deref(),
            Some("moonshotai/kimi-k2.5:free")
        );
        config.default_model = Some("moonshotai/kimi-k2.5".into());
        assert_eq!(
            candidate(&config).unwrap().default_model.as_deref(),
            Some("anthropic/claude-sonnet-5")
        );
    }
    use crate::providers::catalog::{CatalogView, DescriptorSource, Requirement, UnknownPolicy};

    #[test]
    fn retired_invocations_are_unverified_and_new_selections_are_refused() {
        let catalog = CatalogView::release_only();
        let (provider, descriptor, unproven) = crate::providers::catalog::resolver::resolve_model(
            "openai",
            "gpt-4o",
            &Requirement::chat(),
            &catalog,
            UnknownPolicy::Invocation,
        )
        .expect("test: existing model still invokes");
        assert_eq!(provider, ProviderId::of("openai"));
        assert_eq!(descriptor.source, DescriptorSource::LegacyIdOnly);
        assert_ne!(unproven, [] as [&str; 0]);
        for policy in [UnknownPolicy::OperatorSelection, UnknownPolicy::DurableSelection] {
            let error = crate::providers::catalog::resolver::resolve_model(
                "openai",
                "gpt-4o",
                &Requirement::chat(),
                &catalog,
                policy,
            )
            .expect_err("test: retired selection refused");
            assert!(error.to_string().contains("openai/gpt-5.6"));
        }
        assert!(
            crate::providers::catalog::resolver::resolve_model(
                "openai",
                "prx-deleted-model",
                &Requirement::chat(),
                &catalog,
                UnknownPolicy::Invocation,
            )
            .is_ok()
        );
        assert!(
            crate::providers::catalog::resolver::resolve_model(
                "openai",
                "prx-deleted-model",
                &Requirement::chat(),
                &catalog,
                UnknownPolicy::DurableSelection,
            )
            .is_err()
        );
    }

    #[test]
    fn migration_refuses_cross_provider_replacements_and_preserves_deleted_references() {
        let mut release = ReleaseCatalog::embedded_ref().unwrap().clone();
        let row = release.models.iter_mut().find(|row| row.id == "openai/gpt-4o").unwrap();
        row.lifecycle.replacement = Some("anthropic/claude-sonnet-5".into());
        let error = replacement(&release, "openai", "gpt-4o").unwrap_err();
        assert!(error.to_string().contains("provider ownership"));
        assert!(replacement(&release, "openai", "prx-deleted-model").unwrap().is_none());
        assert!(
            crate::config::model_settings::ModelSettings::default()
                .validate("openai/gpt-4o")
                .is_err()
        );
    }

    #[test]
    fn moonshot_legacy_selection_warns_and_migrates_to_the_priced_release_default() {
        let config = Config {
            default_provider: Some("moonshot".into()),
            default_model: Some("kimi-k2.5".into()),
            ..Config::default()
        };
        assert!(
            warnings(&config)
                .unwrap()
                .iter()
                .any(|note| note.contains("moonshot/kimi-k3"))
        );
        let next = candidate(&config).unwrap();
        assert_eq!(next.default_model.as_deref(), Some("kimi-k3"));
        assert_eq!(next.default_provider, config.default_provider);
        assert_eq!(config.default_model.as_deref(), Some("kimi-k2.5"));
    }

    #[test]
    fn route_fallback_migration_uses_its_actual_provider_owners() {
        let mut config = Config {
            default_provider: Some("openai".into()),
            default_model: Some("gpt-5.6".into()),
            ..Config::default()
        };
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "quick".into(),
            provider: "anthropic".into(),
            model: "claude-3-haiku".into(),
            api_key: None,
            credential_ref: None,
            settings: Default::default(),
        });
        config
            .reliability
            .model_fallbacks
            .insert("claude-3-haiku".into(), vec!["claude-opus-4-8".into()]);
        let next = candidate(&config).unwrap();
        assert_eq!(
            next.reliability.model_fallbacks.get("claude-sonnet-5").unwrap(),
            &vec!["claude-opus-4-8"]
        );
        assert_eq!(next.default_model, config.default_model);
        config.reliability.fallback_providers.push("openai".into());
        assert!(
            candidate(&config)
                .unwrap_err()
                .to_string()
                .contains("different replacements")
        );
    }

    #[tokio::test]
    async fn deleted_model_settings_survive_loading_and_doctor_warns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"default_temperature = 0.7
default_provider = "openai"
default_model = "prx-deleted-model"
[auth]
codex_auth_json_auto_import = false
[model_settings."openai/prx-deleted-model"]
level = "high"
"#,
        )
        .unwrap();
        let config = Config::load_from_path(&path, dir.path().join("workspace")).unwrap();
        assert_eq!(config.default_model.as_deref(), Some("prx-deleted-model"));
        assert_eq!(
            config
                .model_settings
                .get("openai/prx-deleted-model")
                .unwrap()
                .level
                .as_deref(),
            Some("high")
        );
        let notes = warnings(&config).unwrap();
        assert!(notes.iter().any(|note| note.contains("deleted or custom")));
        let report = crate::doctor::diagnose(&config);
        assert!(
            report
                .items
                .iter()
                .any(|item| item.message.contains("deleted or custom"))
        );
    }

    #[test]
    fn migration_replaces_default_routes_fallback_keys_and_settings_together() {
        let mut config = Config {
            default_provider: Some("openai".into()),
            default_model: Some("gpt-4o".into()),
            ..Config::default()
        };
        config
            .reliability
            .model_fallbacks
            .insert("gpt-4o".into(), vec!["prx-custom-fallback".into()]);
        config.model_settings.insert(
            "openai/gpt-4o".into(),
            crate::config::model_settings::ModelSettings {
                level: Some("high".into()),
                ..Default::default()
            },
        );
        config.model_routes.push(crate::config::ModelRouteConfig {
            hint: "quick".into(),
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            api_key: None,
            credential_ref: None,
            settings: Default::default(),
        });
        let next = candidate(&config).unwrap();
        assert_eq!(next.default_model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(next.model_routes.first().unwrap().model, "gpt-5.6-sol");
        assert_eq!(
            next.reliability.model_fallbacks.get("gpt-5.6-sol").unwrap(),
            &vec!["prx-custom-fallback"]
        );
        assert_eq!(
            next.model_settings.get("openai/gpt-5.6-sol").unwrap(),
            config.model_settings.get("openai/gpt-4o").unwrap()
        );
        assert!(!next.model_settings.contains_key("openai/gpt-4o"));
        assert_eq!(next.default_provider, config.default_provider);
        assert_eq!(next.api_key, config.api_key);
        let mut collapsed = config.clone();
        collapsed
            .reliability
            .model_fallbacks
            .insert("gpt-4o".into(), vec!["gpt-4o-mini".into()]);
        assert!(
            candidate(&collapsed)
                .unwrap_err()
                .to_string()
                .contains("collapses fallback")
        );
        config
            .model_settings
            .insert("openai/gpt-5.6".into(), Default::default());
        assert!(candidate(&config).unwrap_err().to_string().contains("conflict"));
    }
}
