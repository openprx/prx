//! Cost and latency estimates pinned before one turn's candidate generation.

use std::collections::BTreeMap;

#[cfg(test)]
use crate::causal_tree::branch::CapabilityRef;
use crate::causal_tree::branch::{CapabilityTarget, CostEstimate};
#[cfg(test)]
use crate::causal_tree::candidate_generator::CapabilityCostModel;
use crate::causal_tree::candidate_generator::CostEstimateSource;
use crate::config::Config;
use crate::config::schema::CostConfig;
use crate::providers::catalog::ResolvedModel;
use crate::tools::capability::catalog::CapabilityEntry;
#[cfg(test)]
use crate::tools::capability::catalog::CatalogView;

/// V1 family fallback estimates in milliseconds. Complete single-capability
/// outcomes can replace one capability's default with observed total-turn p50.
pub(super) const LATENCY_DEFAULT_VERSION: &str = "family-latency-v1";

/// Borrowed estimates for only the pinned entries the generator admits.
/// The pricing validity fact is captured once for the turn; a missing price
/// remains unknown to G6 and is never converted to a free estimate.
pub(super) struct RuntimeCostModel<'a> {
    model: &'a ResolvedModel,
    pricing: &'a CostConfig,
    observed_latency_ms: &'a BTreeMap<String, u32>,
    prices_valid: bool,
    zero_price_exempt: bool,
}

impl<'a> RuntimeCostModel<'a> {
    pub(super) fn new(
        model: &'a ResolvedModel,
        config: &'a Config,
        observed_latency_ms: &'a BTreeMap<String, u32>,
    ) -> Self {
        Self {
            model,
            pricing: &config.cost,
            observed_latency_ms,
            prices_valid: config.cost.prices.values().all(|price| price.is_finite_nonnegative()),
            zero_price_exempt: zero_price_exempt(model),
        }
    }
}

impl CostEstimateSource for RuntimeCostModel<'_> {
    fn estimate_entry(&self, entry: &CapabilityEntry) -> Option<(CostEstimate, u32)> {
        let cost = estimate_cost_with_validity(
            self.model,
            self.pricing,
            entry.schema_bytes,
            self.prices_valid,
            self.zero_price_exempt,
        )?;
        let latency = self
            .observed_latency_ms
            .get(entry.id.as_str())
            .copied()
            .unwrap_or_else(|| default_latency_ms(entry.family));
        Some((cost, latency))
    }

    fn estimate_terminal(&self) -> Option<(CostEstimate, u32)> {
        let cost = estimate_cost_with_validity(self.model, self.pricing, 0, self.prices_valid, self.zero_price_exempt)?;
        Some((cost, default_latency_ms(CapabilityTarget::None)))
    }
}

const fn default_latency_ms(target: CapabilityTarget) -> u32 {
    match target {
        CapabilityTarget::None => 250,
        CapabilityTarget::Memory | CapabilityTarget::Session => 40,
        CapabilityTarget::Skill | CapabilityTarget::Hook => 100,
        CapabilityTarget::NativeTool | CapabilityTarget::Filesystem => 150,
        CapabilityTarget::Mcp | CapabilityTarget::Wasm | CapabilityTarget::Document => 300,
        CapabilityTarget::Shell | CapabilityTarget::Web | CapabilityTarget::Im => 500,
    }
}

/// Build one immutable estimate table from the catalog, PMM-selected model,
/// and the pinned generation's pricing policy. Missing pricing stays
/// missing: G6 must not treat an unknown price as zero.
#[cfg(test)]
pub(super) fn build_cost_model(
    catalog: &CatalogView<'_>,
    model: &ResolvedModel,
    pricing: &CostConfig,
    observed_latency_ms: &BTreeMap<String, u32>,
) -> CapabilityCostModel {
    let mut per_capability = BTreeMap::new();
    for entry in catalog.iter_entries() {
        if let Some(cost) = estimate_cost(model, pricing, entry.schema_bytes) {
            per_capability.insert(
                CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                },
                (
                    cost,
                    observed_latency_ms
                        .get(entry.id.as_str())
                        .copied()
                        .unwrap_or_else(|| default_latency_ms(entry.family)),
                ),
            );
        }
    }
    let mut estimates = BTreeMap::new();
    if let Some(cost) = estimate_cost(model, pricing, 0) {
        estimates.insert(
            CapabilityTarget::None,
            (cost, default_latency_ms(CapabilityTarget::None)),
        );
    }
    CapabilityCostModel {
        per_capability,
        estimates,
    }
}

#[cfg(test)]
fn estimate_cost(model: &ResolvedModel, pricing: &CostConfig, schema_bytes: usize) -> Option<CostEstimate> {
    let prices_valid = pricing.prices.values().all(|price| price.is_finite_nonnegative());
    estimate_cost_with_validity(model, pricing, schema_bytes, prices_valid, zero_price_exempt(model))
}

fn zero_price_exempt(model: &ResolvedModel) -> bool {
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

fn estimate_cost_with_validity(
    model: &ResolvedModel,
    pricing: &CostConfig,
    schema_bytes: usize,
    prices_valid: bool,
    zero_price_exempt: bool,
) -> Option<CostEstimate> {
    // Even a local model cannot turn a configured but invalid tariff into a
    // zero-cost fallback. Normal config loading rejects these values; this
    // guard also covers direct/in-memory callers.
    if !prices_valid {
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
        crate::llm::route_decision::UsagePrice::Missing if zero_price_exempt => 0.0,
        crate::llm::route_decision::UsagePrice::Missing | crate::llm::route_decision::UsagePrice::Invalid => {
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
    use crate::tools::capability::catalog::{CapabilityCatalog, CatalogSources, SurfacePolicy};
    use crate::tools::traits::{Tool, ToolResult};
    use async_trait::async_trait;

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

    fn empty_catalog() -> CapabilityCatalog {
        CapabilityCatalog::build(&[], &CatalogSources::default(), &SurfacePolicy::default())
            .expect("empty capability catalog")
    }

    struct SizedSchemaTool {
        name: &'static str,
        payload: String,
    }

    #[async_trait]
    impl Tool for SizedSchemaTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "PRX cost fixture"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{"payload":{"const":self.payload}}})
        }

        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
    }

    #[test]
    fn same_family_schemas_receive_distinct_generation_bound_costs() {
        let config = crate::config::Config::default();
        let model = model(&config, "openai", "gpt-4o");
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(SizedSchemaTool {
                name: "prx_small",
                payload: "a".to_string(),
            }),
            Box::new(SizedSchemaTool {
                name: "prx_large",
                payload: "b".repeat(8_192),
            }),
        ];
        let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default())
            .expect("fixture catalog");
        let view = catalog.view(&Default::default());
        let cost = build_cost_model(&view, &model, &config.cost, &BTreeMap::new());
        let observed_id = view.entries().first().expect("first capability").id.as_str().to_owned();
        let latencies = BTreeMap::from([(observed_id, 37)]);
        let observed = build_cost_model(&view, &model, &config.cost, &latencies);
        let lazy = RuntimeCostModel::new(&model, &config, &latencies);
        assert_eq!(lazy.estimate_terminal(), observed.estimate_terminal());
        for entry in view.entries() {
            assert_eq!(
                entry.schema_bytes,
                serde_json::to_vec(entry.schema.as_ref())
                    .expect("pinned schema serialization")
                    .len()
            );
            assert_eq!(
                lazy.estimate_entry(entry),
                observed.estimate_entry(entry),
                "on-demand estimate must match the exact pinned catalog entry"
            );
        }
        let first = view.entries().first().expect("first capability");
        let first_ref = CapabilityRef {
            id: first.id.clone(),
            fingerprint: first.fingerprint,
            generation: first.generation,
        };
        assert_eq!(
            observed.per_capability.get(&first_ref).expect("observed estimate").1,
            37
        );
        let mut by_name = BTreeMap::new();
        for entry in view.entries() {
            let reference = CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            };
            by_name.insert(
                entry.public_name.as_str(),
                cost.per_capability.get(&reference).expect("exact estimate").0,
            );
        }
        assert_eq!(by_name.len(), 2);
        let large = by_name.get("prx_large").expect("large fixture estimate");
        let small = by_name.get("prx_small").expect("small fixture estimate");
        assert!(large.tokens > small.tokens);
        assert_eq!(cost.estimates.len(), 1, "only the terminal has a family estimate");
    }

    #[test]
    fn g6_admits_release_catalog_price_without_a_config_override() {
        let config = crate::config::Config::default();
        assert!(config.cost.prices.is_empty(), "cost.prices is override-only");
        let provider = crate::providers::selection::default_provider(&config);
        let selected = crate::providers::selection::default_model(&config);
        assert_eq!(provider, "openrouter");
        assert_eq!(selected, "anthropic/claude-sonnet-5");
        let model = model(&config, provider, selected);
        let catalog = empty_catalog();
        let view = catalog.view(&Default::default());
        let priced = build_cost_model(&view, &model, &config.cost, &BTreeMap::new());
        let &(cost, latency) = priced
            .estimates
            .get(&CapabilityTarget::None)
            .expect("priced answer candidate");
        assert_eq!(cost.tokens, 128);
        assert!(cost.micro_usd > 0);
        assert_eq!(latency, default_latency_ms(CapabilityTarget::None));
        assert!(
            RuntimeCostModel::new(&model, &config, &BTreeMap::new())
                .estimate_terminal()
                .is_some()
        );
    }

    #[test]
    fn an_unknown_price_cannot_become_a_free_candidate() {
        let config = crate::config::Config::default();
        let model = model(&config, "openai", "prx-unlisted-model");
        let catalog = empty_catalog();
        let view = catalog.view(&Default::default());
        let unpriced = build_cost_model(&view, &model, &config.cost, &BTreeMap::new());

        assert!(
            unpriced.estimates.is_empty(),
            "an unknown price cannot become a free candidate"
        );
        assert!(
            RuntimeCostModel::new(&model, &config, &BTreeMap::new())
                .estimate_terminal()
                .is_none()
        );
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
    fn ollama_is_zero_money_unless_it_has_an_explicit_price() {
        let config = crate::config::Config::default();
        let model = model(&config, "ollama", "prx-local-model");
        let zero = estimate_cost(&model, &config.cost, 0).expect("local token estimate");
        assert_eq!(zero.tokens, 128);
        assert_eq!(zero.micro_usd, 0);

        let mut priced = config;
        priced.cost.prices.insert(
            "ollama/prx-local-model".to_string(),
            crate::config::schema::ModelPricing {
                input: 1.0,
                output: 1.0,
                cache_write: 0.0,
                cache_read: 0.0,
            },
        );
        assert!(
            estimate_cost(&model, &priced.cost, 0)
                .expect("explicit local price")
                .micro_usd
                > 0
        );
        priced
            .cost
            .prices
            .get_mut("ollama/prx-local-model")
            .expect("local price")
            .input = -1.0;
        assert!(
            estimate_cost(&model, &priced.cost, 0).is_none(),
            "invalid tariff must not become local free pricing"
        );
        assert!(
            RuntimeCostModel::new(&model, &priced, &BTreeMap::new())
                .estimate_terminal()
                .is_none()
        );
        priced
            .cost
            .prices
            .get_mut("ollama/prx-local-model")
            .expect("local price")
            .input = f64::MAX;
        assert!(
            estimate_cost(&model, &priced.cost, 0).is_none(),
            "overflowed tariff must not become local free pricing"
        );
        priced
            .cost
            .prices
            .get_mut("ollama/prx-local-model")
            .expect("local price")
            .input = f64::from_bits(1);
        priced
            .cost
            .prices
            .get_mut("ollama/prx-local-model")
            .expect("local price")
            .output = f64::from_bits(1);
        assert!(
            estimate_cost(&model, &priced.cost, 0).is_none(),
            "positive subnormal tariff must not become local free pricing"
        );
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

    #[test]
    fn defaults_have_a_versioned_value_for_every_target() {
        for target in CapabilityTarget::ALL {
            assert!(default_latency_ms(target) > 0);
        }
    }
}
