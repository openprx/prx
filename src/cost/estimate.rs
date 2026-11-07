//! Pure per-call cost estimation shared by the G6 candidate model and send admission.

use crate::causal_tree::branch::CostEstimate;
use crate::config::schema::CostConfig;
use crate::providers::catalog::ResolvedModel;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(test)]
pub(crate) fn estimate_cost(model: &ResolvedModel, pricing: &CostConfig, schema_bytes: usize) -> Option<CostEstimate> {
    let prices_valid = pricing.prices.values().all(|price| price.is_finite_nonnegative());
    estimate_cost_with_validity(
        model,
        pricing,
        schema_bytes,
        prices_valid,
        zero_price_exempt(model),
        &AtomicBool::new(false),
        &AtomicBool::new(false),
    )
}

pub(crate) fn zero_price_exempt(model: &ResolvedModel) -> bool {
    provider_is_zero_price_exempt(model.provider(), model.credential().api_url())
        || (cfg!(test) && model.provider().catalog_name() == Some("mock"))
}

/// The one provider rule for "no price row means no money": a local Ollama
/// model is free unless it has an explicit price. "Local" means the effective
/// base URL is unset (the default `http://localhost:11434`) or a literal
/// loopback address; a remote Ollama (`https://ollama.com`, a proxy, a LAN
/// host) can bill, so an unpriced one is never free. G6 estimates, fallback
/// admission and decision-attempt reservation/settlement all use it.
pub(crate) fn provider_is_zero_price_exempt(provider: &crate::providers::ProviderId, api_url: Option<&str>) -> bool {
    provider.catalog_name() == Some("ollama")
        && api_url
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .is_none_or(crate::decision::loopback::is_literal_loopback)
}

/// One G6 refusal diagnostic per turn, shared with the missing-price warning.
pub(crate) fn warn_once(warned: &AtomicBool, message: impl FnOnce() -> String) {
    if !warned.swap(true, Ordering::Relaxed) {
        tracing::warn!(target: "providers::selection", "{}", message());
    }
}

pub(crate) fn estimate_cost_with_validity(
    model: &ResolvedModel,
    pricing: &CostConfig,
    schema_bytes: usize,
    prices_valid: bool,
    zero_price_exempt: bool,
    warned_unpriced: &AtomicBool,
    noted_subscription: &AtomicBool,
) -> Option<CostEstimate> {
    // Even a local model cannot turn a configured but invalid tariff into a
    // zero-cost fallback. Normal config loading rejects these values; this
    // guard also covers direct/in-memory callers.
    if !prices_valid {
        warn_once(warned_unpriced, || {
            let key = pricing
                .prices
                .iter()
                .find(|(_, price)| !price.is_finite_nonnegative())
                .map_or("<unknown>", |(key, _)| key.as_str());
            format!(
                "G6 budget admission refuses every priced candidate: [cost.prices.{:?}] has a negative or non-finite component; fix the price (USD per 1M tokens, finite and not negative)",
                crate::security::redaction::redact_provider_text(key)
            )
        });
        return None;
    }
    // V1 token estimate: 64 prompt tokens, at most four schema bytes per
    // token, and 64 completion tokens. Saturation remains over budget.
    let input_tokens = 64_u64.saturating_add(u64::try_from(schema_bytes.saturating_add(3) / 4).unwrap_or(u64::MAX));
    let total_tokens = input_tokens.saturating_add(64);
    let price = crate::llm::route_decision::usage_cost_resolution(
        model.provider_label(),
        model.model(),
        input_tokens,
        64,
        0,
        0,
        pricing,
    );
    let usd = match price {
        crate::llm::route_decision::UsagePrice::Priced(cost) => cost,
        crate::llm::route_decision::UsagePrice::Subscription => {
            if !noted_subscription.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    target: "providers::selection",
                    "{}",
                    crate::providers::selection::SUBSCRIPTION_BILLING_NOTICE
                );
            }
            0.0
        }
        crate::llm::route_decision::UsagePrice::Missing if zero_price_exempt => 0.0,
        crate::llm::route_decision::UsagePrice::Missing => {
            if !warned_unpriced.swap(true, Ordering::Relaxed)
                && let Some(warning) = crate::providers::selection::pricing_warning(model, pricing)
            {
                tracing::warn!(target: "providers::selection", "{warning}");
            }
            return None;
        }
        crate::llm::route_decision::UsagePrice::Invalid => {
            warn_once(warned_unpriced, || {
                format!(
                    "G6 budget admission refuses model '{}/{}': its price or token count is invalid (negative, non-finite or overflowing); fix [cost.prices] for this model",
                    crate::security::redaction::redact_provider_text(model.provider_label()),
                    crate::security::redaction::redact_provider_text(model.model())
                )
            });
            return None;
        }
    };
    let micro = u64::try_from(crate::cost::tracker::ceil_micro_usd(usd).ok()?).ok()?;
    Some(CostEstimate {
        tokens: u32::try_from(total_tokens).unwrap_or(u32::MAX),
        micro_usd: micro,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::catalog::{
        CatalogView as ProviderCatalogView, Requirement, Selection, UnknownPolicy, resolve,
    };
    use crate::providers::credentials::{ConfigCredentialSource, CredentialRef};

    fn model(config: &crate::config::Config, provider: &str, model: &str) -> ResolvedModel {
        resolve(
            &Selection {
                provider,
                model,
                credential: CredentialRef::ProviderStore,
            },
            &Requirement::chat(),
            &ProviderCatalogView::from_config(config),
            &ConfigCredentialSource::new(config),
            UnknownPolicy::Invocation,
        )
        .expect("PMM chat resolution")
    }

    #[test]
    fn estimate_rounds_recorded_price_up_at_micro_boundary() {
        let mut config = crate::config::Config::default();
        config.cost.prices.insert(
            "openai/gpt-4o".to_string(),
            crate::config::schema::ModelPricing {
                input: 0.023_437_5,
                output: 0.023_437_5,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        let model = model(&config, "openai", "gpt-4o");
        let recorded_usd = crate::llm::route_decision::usage_cost_usd("openai", "gpt-4o", 64, 64, 0, 0, &config.cost)
            .expect("configured price");
        let expected = crate::cost::tracker::ceil_micro_usd(recorded_usd).expect("recorded amount");
        let estimate = estimate_cost(&model, &config.cost, 0).expect("priced candidate");
        assert_eq!(expected, 4);
        assert_eq!(u128::from(estimate.micro_usd), expected);
    }

    #[test]
    fn a_remote_ollama_is_not_free_when_unpriced() {
        let id = crate::providers::ProviderId::of("ollama");
        assert!(provider_is_zero_price_exempt(&id, None));
        assert!(provider_is_zero_price_exempt(&id, Some("")));
        assert!(provider_is_zero_price_exempt(&id, Some("http://localhost:11434")));
        assert!(provider_is_zero_price_exempt(&id, Some("http://127.0.0.1:11434")));
        assert!(provider_is_zero_price_exempt(&id, Some("http://[::1]:11434")));
        assert!(!provider_is_zero_price_exempt(&id, Some("https://ollama.com")));
        assert!(!provider_is_zero_price_exempt(&id, Some("http://192.168.1.20:11434")));
        assert!(!provider_is_zero_price_exempt(
            &crate::providers::ProviderId::of("openai"),
            None
        ));

        // Through G6: the resolved model's effective base URL decides.
        let mut config = crate::config::Config::default();
        config.default_provider = Some("ollama".to_string());
        config.api_url = Some("https://ollama.com".to_string());
        let remote = resolve(
            &Selection {
                provider: "ollama",
                model: "prx-local-model",
                credential: CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root),
            },
            &Requirement::chat(),
            &ProviderCatalogView::from_config(&config),
            &ConfigCredentialSource::new(&config),
            UnknownPolicy::Invocation,
        )
        .expect("remote ollama resolves");
        assert!(estimate_cost(&remote, &config.cost, 0).is_none());
    }

    #[test]
    fn mock_is_zero_money_without_an_external_tariff() {
        let config = crate::config::Config::default();
        let model = model(&config, "mock", "prx-local-model");
        let estimate = estimate_cost(&model, &config.cost, 0).expect("local mock estimate");
        assert_eq!(estimate.tokens, 128);
        assert_eq!(estimate.micro_usd, 0);
    }

    #[test]
    fn huge_schema_saturates_over_budget_instead_of_wrapping_to_zero() {
        let config = crate::config::Config::default();
        let model = model(&config, "openai", "gpt-4o");
        let result = estimate_cost(&model, &config.cost, usize::MAX);
        assert!(result.is_none_or(|cost| cost.tokens == u32::MAX && cost.micro_usd > 1_000_000));
    }
}
