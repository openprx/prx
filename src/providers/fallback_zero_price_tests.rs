#![cfg(test)]
use super::*;
use crate::providers::catalog::{Requirement, Selection, UnknownPolicy, resolve};
use crate::providers::credentials::{ConfigCredentialSource, CredentialRef, LegacySlot};

fn sibling_is_cost_admitted(api_url: Option<&str>) -> bool {
    let mut config = crate::config::Config::default();
    config.default_provider = Some("ollama".to_string());
    config.api_url = api_url.map(str::to_string);
    config.api_key = Some("test-ollama-root-key".to_string());
    let catalog = catalog::CatalogView::from_config(&config);
    let resolved = resolve(
        &Selection {
            provider: "ollama",
            model: "prx-local-model",
            credential: CredentialRef::LegacyInline(LegacySlot::Root),
        },
        &Requirement::chat(),
        &catalog,
        &ConfigCredentialSource::new(&config),
        UnknownPolicy::Invocation,
    )
    .expect("test: ollama primary resolves");
    let mut reliability = config.reliability.clone();
    reliability
        .model_fallbacks
        .insert("prx-local-model".to_string(), vec!["prx-unpriced-sibling".to_string()]);
    let admission = fallback_admission_for(&resolved, &catalog, &reliability, &config.cost);
    admission
        .eligibility(&ProviderId::of("ollama"), "prx-unpriced-sibling")
        .expect("test: sibling is described")
        .cost_admitted
}

#[test]
fn an_unpriced_sibling_is_free_only_on_a_local_ollama_primary() {
    assert!(sibling_is_cost_admitted(None));
    assert!(sibling_is_cost_admitted(Some("http://localhost:11434")));
    assert!(!sibling_is_cost_admitted(Some("https://ollama.com")));
}
