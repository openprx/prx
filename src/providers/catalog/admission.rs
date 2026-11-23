//! Admission of newly configured models using release or operator evidence.
use super::CatalogView;
use crate::{config::Config, providers::ProviderId};

/// A model selection without a release entry or capability declaration.
#[derive(Debug, thiserror::Error)]
#[error(
    "model_capabilities_unknown: declare capabilities for model '{model}' in [[providers.models]]; for prx init choose exactly one of --native-tools or --no-native-tools; nothing was written"
)]
pub struct ModelCapabilitiesUnknown {
    model: String,
}

impl ModelCapabilitiesUnknown {
    #[must_use]
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
        }
    }
}

/// Require local catalog or operator evidence before recording a selection.
/// Existing on-disk selections are not migrated or changed.
pub fn check_declared(config: &Config, provider: &str, model: &str) -> Result<(), ModelCapabilitiesUnknown> {
    let catalog = CatalogView::from_config(config);
    let model = model.trim();
    let (provider, model) = model
        .strip_prefix("hint:")
        .and_then(|hint| catalog.route(hint))
        .map_or((provider, model), |route| {
            (route.provider.as_str(), route.model.as_str())
        });
    let id = ProviderId::of(provider);
    let released = catalog
        .release_catalog()
        .is_ok_and(|release| release.model(&id, model).is_some());
    let declared = config.providers.models.iter().any(|entry| {
        ProviderId::of(&entry.provider) == id
            && entry
                .model
                .as_deref()
                .is_none_or(|name| name.trim().eq_ignore_ascii_case(model))
            && entry.declares_anything()
    });
    if released || declared {
        Ok(())
    } else {
        Err(ModelCapabilitiesUnknown::new(model))
    }
}
