//! Model settings command logic; the chat loop owns the actual hot swap.
use crate::config::Config;
use crate::providers::{ProviderId, ProviderRuntimeOptions, catalog::ReleaseCatalog};
use anyhow::{Context as _, Result};
use std::sync::Arc;

pub(super) async fn save_default(
    config: &Config,
    provider: &str,
    model: &str,
    options: &ProviderRuntimeOptions,
) -> Result<()> {
    use crate::config::manage::{ConfigActor, ManagedChange, ManagementService, RootCredential, SelectionOperation};
    let release = ReleaseCatalog::embedded_ref()?;
    let settings = release
        .model(&ProviderId::of(provider), model)
        .and_then(|row| {
            options
                .model_settings
                .as_ref()
                .and_then(|settings| settings.get(&row.id))
        })
        .cloned();
    let config_path = config.config_path.clone();
    let workspace = config.workspace_dir.clone();
    let current =
        crate::runtime::blocking::spawn_blocking(move || Config::load_from_path(&config_path, workspace)).await??;
    let service = ManagementService::deferred(crate::config::new_shared(current));
    let change = ManagedChange::SetDefault {
        operation: SelectionOperation::Chat,
        provider: provider.to_owned(),
        model: model.to_owned(),
        root_credential: RootCredential::Refuse,
        settings,
    };
    let plan = service.plan(&change, ConfigActor::Cli).await?;
    service.apply(plan, ConfigActor::Cli, None).await?;
    Ok(())
}

pub(super) fn selection(
    input: &str,
    current_model: &str,
    provider: &str,
    options: &ProviderRuntimeOptions,
) -> Result<(String, ProviderRuntimeOptions)> {
    let is_level = input.starts_with("/level ");
    let mut parts = input
        .strip_prefix(if is_level { "/level " } else { "/model " })
        .unwrap_or_default()
        .split_whitespace();
    let first = parts.next();
    let second = parts.next();
    let (model, level) = if is_level {
        (current_model, first)
    } else {
        (first.unwrap_or_default(), second)
    };
    anyhow::ensure!(
        !model.is_empty() && parts.next().is_none() && (!is_level || first.is_some() && second.is_none()),
        "Usage: /model <name> [low|medium|high] or /level <low|medium|high>"
    );
    let mut candidate = options.clone();
    if let Some(level) = level {
        let selected = (|| -> Result<()> {
            let release = ReleaseCatalog::embedded_ref()?;
            let row = release
                .model(&ProviderId::of(provider), model)
                .ok_or_else(|| anyhow::anyhow!("model levels require a release catalog model"))?;
            // Only the new level is a caller choice. Stale saved parameters
            // continue through runtime compatibility handling.
            crate::config::model_settings::ModelSettings {
                level: Some(level.to_owned()),
                ..Default::default()
            }
            .validate(&row.id)?;
            let models = candidate
                .model_settings
                .get_or_insert_with(|| Arc::new(Default::default()));
            let settings = Arc::make_mut(models).entry(row.id.clone()).or_default();
            settings.level = Some(level.to_owned());
            settings.overrides.remove("level");
            Ok(())
        })();
        selected.context("Cannot select model level")?;
    }
    Ok((model.to_owned(), candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_level_parser_preserves_usage_errors_and_selects_transient_levels() {
        let options = ProviderRuntimeOptions::default();
        for input in [
            "/model ",
            "/level ",
            "/model gpt-5.6 high extra",
            "/level high extra",
            "/level future",
        ] {
            assert!(selection(input, "gpt-5.6", "openai", &options).is_err(), "{input}");
        }
        let (model, selected) = selection("/level high", "gpt-5.6", "openai", &options).unwrap();
        assert_eq!(model, "gpt-5.6");
        assert_eq!(
            selected
                .model_settings
                .unwrap()
                .get("openai/gpt-5.6")
                .unwrap()
                .level
                .as_deref(),
            Some("high")
        );
        assert!(options.model_settings.is_none());
    }
}
