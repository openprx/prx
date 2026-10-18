#![cfg(test)]

use super::*;

#[test]
fn an_operator_tariff_can_admit_an_active_router_row_without_reviving_a_retired_row() {
    let mut catalog = crate::providers::catalog::ReleaseCatalog::embedded_ref()
        .expect("release catalog")
        .clone();
    let template = catalog
        .models
        .iter()
        .find(|row| row.id == "openrouter/anthropic/claude-sonnet-5")
        .expect("router template");
    let router = template.router.clone();
    let limits = template.limits.clone();
    let row = catalog
        .models
        .iter_mut()
        .find(|row| row.id == "openrouter/google/gemini-3.5-flash")
        .expect("unpriced active row");
    assert!(row.pricing.is_none());
    row.router = router;
    row.limits = limits;
    let mut cost = crate::config::schema::CostConfig::default();
    let candidate = |models: &[RouterModelConfig]| {
        models
            .iter()
            .any(|row| row.provider == "openrouter" && row.model_id == "google/gemini-3.5-flash")
    };
    assert!(!candidate(&model_capabilities_from_catalog(&catalog, &cost)));
    // Slash-qualified model IDs are already the billing override key.
    cost.prices.insert(
        "openrouter/google/gemini-3.5-flash".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 2.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
    );
    assert!(!candidate(&model_capabilities_from_catalog(&catalog, &cost)));
    for key in ["google/gemini-3.5-flash", "openai/gpt-4o"] {
        cost.prices.insert(
            key.to_owned(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 2.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
    }
    let models = model_capabilities_from_catalog(&catalog, &cost);
    assert!(candidate(&models));
    assert!(
        !models
            .iter()
            .any(|row| row.provider == "openai" && row.model_id == "gpt-4o")
    );
}
