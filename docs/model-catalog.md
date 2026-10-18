# Release model catalog

PRX ships one model catalog in every binary: [`catalog/models.json`](../catalog/models.json).
The file is embedded with the release, then loaded and validated once per
process. Release tests validate the shipped document before packaging. PRX does
not download or overlay catalog files at runtime. Catalog changes therefore
arrive with a PRX release and use the normal binary rollback path.

The catalog supplies the offline baseline for provider defaults, onboarding,
`prx models list`, Ollama Cloud's offline fallback list, provider/model
descriptors, prices, context windows, Router candidates, model features, and
hosted decision transport advice. A live provider catalog is separate discovery
evidence. User and operator choices stay in TOML, including
`[[providers.models]]` declarations; they are never written back into the
release JSON.

## Format and validation

[`catalog/models.schema.json`](../catalog/models.schema.json) defines schema
version 1. Each document has a release `revision`, publication timestamp,
provider defaults, a global default, and model rows. A row can contain:

- canonical `provider/model` identity, adapter-facing `api_model`, aliases,
  display name, and lifecycle state;
- supported operations and chat feature facts;
- token limits, tokenizer name, prices, router hints, and transport advice;
- declarations from the closed request-parameter vocabulary; and
- model levels named `low`, `medium`, and `high`, plus `default_level`.

Every chat model defines all three common model levels, even when a level does
not change any parameters yet. A level is a bundle of typed parameter values;
it is distinct from a provider's `reasoning_effort` value.

The loader rejects malformed JSON, unknown schema-v1 fields, unsupported schema
majors, invalid model or provider defaults, duplicate IDs, aliases that collide
with an ID or API model for the same provider, missing lifecycle replacements,
invalid prices and limits, invalid level values, credential fields, secret-like
strings, and URLs containing user information. Newer schema minors load with a
warning. Unknown future parameter names are ignored with a warning and never
reach a provider request. Loader warnings are emitted once when the embedded
catalog is first opened.

New selections show active chat rows only. Retired rows remain available as
facts for configurations that already name them but are absent from onboarding
and the default `models list`; decision-only rows are also absent from those chat
pickers. Each provider's declared default is the first and preselected entry,
independent of JSON ordering or a live provider list.

Catalog capability evidence uses the same resolver path as operator and live
facts. The order is:

1. adapter ceiling;
2. negative live discovery;
3. `[[providers.models]]` operator declaration;
4. positive live discovery;
5. embedded release catalog;
6. unverified model ID.

The effective descriptor records `Catalog { revision }` when the release row
is its evidence. A child process snapshot records both the revision and SHA-256
digest of the embedded JSON and refuses to restore under a binary with a
different revision or content.

## Runtime consumers and overrides

The embedded catalog is the sole built-in source for the unset provider/model,
model prices, context windows, Router hints, and model-level tool and vision
features. Adapter capabilities remain an upper bound: a positive catalog fact
cannot enable a feature the adapter cannot send. Negative operator or discovery
evidence continues to outrank a positive release fact.

`[cost.prices]` contains operator overrides only. An exact or redacted endpoint
override wins; otherwise usage accounting and the G6 admission gate read the
release price. A missing catalog and override price remains unpriced, with the
existing Ollama-only zero-price exemption. OpenRouter rows without a duplicated
tariff resolve stable upstream Anthropic, Google/Gemini, Moonshot, OpenAI and
xAI model prices from those providers' release rows; unknown or dynamic proxy
models remain unpriced. Compaction reads the effective
descriptor's context window after explicit agent and `[[router.models]]`
overrides. The default Router candidate list uses active rows with `router`
hints and an effective billing tariff, including configured `[cost.prices]`
overrides. Doctor applies the same configured-price filter. A price override
does not revive a retired row or add absent Router metadata.
`router.cost_per_million_tokens` is a comparative ranking input, not a billing
tariff; billing uses the separate catalog price or operator override.

The `ollama/*` row exists only to seed the Router's conservative local fallback.
It does not verify an arbitrary Ollama model, assign it a 32K context window, or
deny its vision support. Exact release, discovery, or operator facts determine
those properties; otherwise they stay unknown and adapter-bounded.

Image admission intersects the concrete adapter ceiling with the effective
model descriptor before provider transport. The same intersection controls
native-tool publication on the turn path. Compatible endpoints no longer infer
vision from model-name or URL substrings; declare a custom endpoint's facts in
`[[providers.models]]` when discovery cannot prove them.

When `prx causal config set backend` selects a release-catalog decision model
without explicit transport arguments, the transaction writes the catalog's
pinned snapshot and recommended timeout and, while the policy still has its
implicit latency budget, the catalog's recommended budget. A stored
`extra_latency_budget_ms` remains explicit even when it equals the built-in
default. Explicit command or policy values remain TOML choices. Rollback uses the normal configuration
history; reverting the PRX binary reverts the embedded catalog.

## Maintaining the release file

Edit `catalog/models.json`; do not add a second built-in model table in Rust.
Keep provider defaults pointed at active chat rows, keep aliases unambiguous
within their provider, and keep prices non-negative. Model IDs and model facts,
including the Ollama Cloud fallback set, belong in this JSON; adapter code
retains only protocol wire mapping and transport limits. Increment `revision`
for every content change; process snapshots bind the digest as an independent
check against a missed revision bump.

Run the semantic and real-path checks through the repository guard:

```bash
./scripts/ci/test_home_guard.sh --locked --all-features --no-fail-fast providers::catalog
./scripts/ci/test_home_guard.sh --locked --all-features --no-fail-fast models_list_by_operation_and_show_use_the_service_read_model
./scripts/ci/test_home_guard.sh --locked --all-features --no-fail-fast g6_admits_release_catalog_price_without_a_config_override
./scripts/ci/test_home_guard.sh --locked --all-features --no-fail-fast image_turn_is_gated_by_catalog_vision_input_before_provider_transport
```

The first command validates the embedded document, common levels, aliases,
secrets, evidence precedence, and child revision pinning. The second drives the
real CLI model listing through the management service and proves release rows
carry catalog provenance.
