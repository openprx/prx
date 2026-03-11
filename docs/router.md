# LLM Router

This document covers OpenPRX LLM Router capabilities delivered on 2026-03-10 to 2026-03-11:

- Heuristic routing: capability + Elo + cost + latency
- KNN semantic routing: cold-start guard + timeout-safe fallback
- Automix: cheap-first inference with low-confidence escalation

## Feature Switches

- `router.enabled`: enable heuristic routing
- `router.knn_enabled`: enable KNN semantic similarity scoring
- `router.automix.enabled`: enable confidence-based model escalation

## End-to-End Flow

Text flow (runtime path):

`intent_classify -> select_model -> reliability_fallback -> automix -> record_outcome`

Step details:

1. `intent_classify`
   Classify message intent (`conversation`, `analysis`, `code`, etc.).
1. `select_model`
   Router computes candidate score:
   `alpha*similarity + beta*capability + gamma*elo - delta*cost - epsilon*latency`.
1. `reliability_fallback`
   If the selected model/provider fails, standard reliability fallback applies.
1. `automix`
   If selected model is in cheap tier and confidence is below threshold, upgrade to premium model.
1. `record_outcome`
   Persist success/latency, update Elo/success metrics, and write KNN history.

## Full Configuration Example

```toml
# Required provider defaults for normal request path
[general]
default_provider = "openrouter"
default_model = "openai/gpt-4o-mini"

# Optional: model routes for extra providers/models.
# Reachability filtering only keeps router models that are reachable via:
# - default_provider, or
# - entries declared in [[model_routes]].
[[model_routes]]
hint = "premium"
provider = "openai"
model = "gpt-4.1"

[reliability]
provider_retries = 2
provider_backoff_ms = 800
fallback_providers = ["openrouter"]
[reliability.model_fallbacks]
"gpt-4.1" = ["gpt-4o-mini"]
"claude-sonnet-4-6" = ["claude-haiku-4-5"]

[router]
enabled = true
alpha = 0.0
beta = 0.5
gamma = 0.3
delta = 0.1
epsilon = 0.1
knn_enabled = true
knn_min_records = 10
knn_k = 7

[router.automix]
enabled = true
confidence_threshold = 0.70
cheap_model_tiers = ["cheap", "fast", "mini"]
premium_model_id = "openai/gpt-4.1"

[[router.models]]
model_id = "gpt-4o-mini"
provider = "openrouter"
cost_per_million_tokens = 0.60
max_context = 128000
latency_ms = 1200
categories = ["conversation", "code", "analysis"]
elo_rating = 1000.0

[[router.models]]
model_id = "gpt-4.1"
provider = "openai"
cost_per_million_tokens = 10.0
max_context = 128000
latency_ms = 2200
categories = ["analysis", "code"]
elo_rating = 1000.0
```

## Minimum Viable Configuration

At least one healthy provider must be configured and reachable.

```toml
[general]
default_provider = "openrouter"
default_model = "openai/gpt-4o-mini"

[router]
enabled = true
knn_enabled = false

[router.automix]
enabled = false

[[router.models]]
model_id = "gpt-4o-mini"
provider = "openrouter"
categories = ["conversation"]
```

Operational note:

- Ensure provider credentials and network are valid.
- Verify health with `openprx channel doctor` and your deployment health checks before enabling router in production.

## Field Reference

`[router]`

- `enabled`: master switch for heuristic routing
- `alpha`: semantic similarity weight (KNN score)
- `beta`: capability/category weight
- `gamma`: Elo weight
- `delta`: cost penalty coefficient
- `epsilon`: latency penalty coefficient
- `knn_enabled`: enable semantic KNN lookup
- `knn_min_records`: minimum successful history records before KNN contributes
- `knn_k`: nearest neighbors used for KNN voting
- `models`: static candidate registry

`[[router.models]]`

- `model_id`: model identifier (without provider prefix)
- `provider`: provider id
- `cost_per_million_tokens`: USD cost estimate
- `max_context`: max prompt context tokens
- `latency_ms`: baseline latency estimate
- `categories`: model capability categories used by intent match
- `elo_rating`: initial Elo baseline

`[router.automix]`

- `enabled`: enable Automix escalation
- `confidence_threshold`: escalate when confidence `< threshold`
- `cheap_model_tiers`: markers used to classify cheap-first targets
- `premium_model_id`: escalation target model id (`provider/model` supported)

## Security and Boundary Notes

Audit-driven boundaries introduced with Router rollout:

- Provider reachability filtering:
  Router keeps only models reachable from `general.default_provider` or declared `[[model_routes]]`.
  If no reachable models remain, router auto-disables instead of routing unsafely.
- `record_outcome` lock-free async persistence:
  in-memory metric mutation is completed under write lock, and all async persistence (`memory.store`) happens after lock release (no `await` under lock).
- Reserved `router/` namespace ACL:
  reads/writes for reserved `router/` memory require `session_id="self_system"`.
  Non-`self_system` access is denied at memory backend/tool validation layers.

## Operations Guidance

- Start with `knn_enabled = false` for cold deployments, then enable after enough traffic.
- Keep `knn_min_records >= 10` to avoid unstable early routing.
- Keep `confidence_threshold` conservative (`0.65~0.80`) and monitor premium escalation rate.
