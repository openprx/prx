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

Request parameter declarations use the closed vocabulary `temperature`,
`top_p`, `max_output_tokens`, `reasoning_effort`, `thinking_budget`, `verbosity`,
`service_tier`, and `seed`. Resolution starts with parameter defaults and the
model's default level, then the selected level (the route level when present,
otherwise the model level), explicit model parameters, explicit route parameters,
and explicit caller values. An explicit model `reasoning_effort = "low"` therefore
survives a route `level = "high"` without explicit route parameters. A later value removes
mutually exclusive earlier values, including exclusions declared in the reverse
direction. Unsupported or undeclared overrides are dropped; numeric values are
clamped to the declared range and step. Invalid types, fractional token counts,
unknown levels and unrecognized enum values fail before transport. Recognized
reasoning-effort values outside a model's supported subset clamp to the nearest
declared effort, preferring the higher effort on a tie so `minimal` cannot
disable reasoning. The adapter contains no model-ID branches for clamping.
`runtime.codex_reasoning_effort` accepts only `none`, `minimal`, `low`, `medium`,
`high`, `xhigh`, or `max`; invalid values fail typed configuration validation.
Unknown Codex models retain the legacy `medium` verbosity and `xhigh` effort
base request unless effort is explicitly set. Known Codex models now default to
`medium` effort instead of the pre-C2 `xhigh`; this is an adopted behavior change.
An explicit configured effort still takes precedence.

Adapter code maps canonical values to API fields; JSON metadata cannot add
arbitrary request fields. The request serializer applies the same resolution to
buffered and streaming requests, Gemini OAuth envelopes and signed Bedrock
payloads. The accounted OpenAI and compatible paths estimate and send the same
serialized body. A declared parameter that an adapter cannot express refuses
selection and serialization locally. Bring-your-own identities retain their own
catalog identity, including `anthropic-custom:`; using an Anthropic-compatible
protocol does not borrow original-provider model facts. Unverified model IDs use
the adapter's base request without catalog rewriting. For a catalog model, every
canonical wire field is removed before the declared, resolved values are written;
undeclared fields cannot survive from an adapter default or caller override. An
empty declaration set is unverified and preserves the whole adapter base
request, including the classifier's explicit temperature. For nonempty
declarations, all explicit caller fields (including output limit, top-p and
seed) participate in resolution before unsupported fields are removed.
Ollama's boolean `think` is adapter-owned and survives catalog rewriting.
Anthropic effort maps only to `output_config.effort`; it does not activate
adaptive thinking, because signed thinking-block replay is not implemented.
Anthropic and Bedrock thinking budgets must be strictly less than the output
limit. Other adapters retain their own output-cutoff semantics.
Compatible providers bind lookup to their configured canonical identity, not
their display name. A streaming usage-option retry retains one preparation
record for the logical request. The resolved values and catalog revision
are retained in engine traces and the existing owner-bound
`provider.final_outcome` event when that entrypoint records provider outcomes.
The issued per-run decision context owns main preparation records, so an
interrupted request keeps them in the existing `turn.finalized` payload even
when no provider outcome was observed. A failed or cancelled preparation does
not create an attempt, serving attribution or usage settlement. All entrypoints,
including terminal Chat, use this common finalizer. Nonempty parameter records
are included in the terminal claim and replay authentication; empty records
preserve the existing payload shape.
Signal vision, smart-group classification and Automix premium calls retain their
own prepared values in the existing `provider.auxiliary.settled` receipt, including
on failure. Their accounting owners use independent capture scopes; an outer
turn cannot absorb an auxiliary call's records. The classifier's silence outcome
uses that same owned capture. These records add no separate event write.
They describe request preparation, not confirmed transport or billed usage;
physical attempt and cost records retain those roles. Records include only the
canonical model, release revision and resolved parameter values. Concurrent
turns and nested streaming tasks keep separate captures, and disabled recording
does not borrow an outer turn's recorder. Selection surfaces persist TOML
settings through the PMM transactions described below.

Release `2026-10-19.2` carries Kimi Code's fixed temperature, Codex
effort/verbosity declarations, and explicit declarations for the shipped OpenAI,
Anthropic, Gemini, Ollama, Bedrock and selected OpenRouter rows. Output defaults
and conservative bounds preserve PRX's existing release policy; they do not
claim every vendor-supported output size. Undeclared model facts remain unknown.
The wire fields follow the provider APIs, including Ollama's
[`options` and model-defined `think`](https://docs.ollama.com/api/chat) and
Bedrock's [`inferenceConfig` and `additionalModelRequestFields`](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Converse.html). Declared Codex rows now use the catalog's
`medium` default effort and verbosity; the legacy configured effort remains a
caller override, validated against the selected row. Kimi's rule follows the
selected provider and model; a URL containing `kimi.com` supplies no parameter evidence. The former test-only hidden `kimi-code/kimi-k2` row has been removed; that
legacy spelling has no declared parameter support and gains no factory support.
Newly recorded deprecated Codex rows use this release's annotation date; it is
not a claim about the vendor's original announcement date. Their parameter
subsets preserve the former adapter assertions. See the official
[GPT-5.3-Codex](https://developers.openai.com/api/docs/models/gpt-5.3-codex),
[GPT-5.1](https://developers.openai.com/api/docs/models/gpt-5.1), and
[GPT-5.1-Codex Mini](https://developers.openai.com/api/docs/models/gpt-5.1-codex-mini)
references for the model identities and lifecycle notices.
The Sonnet 5 rows retain all five provider effort values, including `xhigh` and
`max`, as documented in [Claude effort](https://platform.claude.com/docs/en/build-with-claude/effort).
The three model-level bundles remain `low`, `medium` and `high`; provider effort
values are a separate vocabulary.

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

Before reading scoped history, the selected Router decision is committed and
read back from SQLite in one writer operation. The returned row must pass the
same owner, lineage and digest checks within the existing 100 ms history
deadline; a queued write or an expired acknowledgement grants no read.
Ordinary background phase-event writes wait until that hop finishes, including
on cancellation. Observers remain immediate, queued events keep their order,
and cleanup still drains them; the selected decision commit is never deferred.
History reads retain exact finite vector bits and validate every eligible row
through the existing 1,024-row capacity limit. The backend counts the eligible
set before materialization, then applies the bounded column projection and full
row decoder without a duplicate metadata-validation scan.

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

Finding 98 rollback: reverting the fix commits restores the C2 adapter
serialization and pending-allowance lifecycle. Ordinary-child allowance
carving is deferred to the next phase;
ordinary children retain independent per-child allowances during closeout.

## Saving a model level

Model choices remain in TOML. Use a canonical catalog ID as the settings key:

```toml
[model_settings."openai/gpt-5.6"]
level = "high"
max_output_tokens = 64000

[[model_routes]]
hint = "quick"
provider = "openai"
model = "gpt-5.6"
level = "low"
```

Inspect the catalog and plan a change before applying it:

```bash
prx models levels openai/gpt-5.6
prx models settings openai/gpt-5.6
prx models settings openai/gpt-5.6 --level high --set max_output_tokens=64000
prx models settings openai/gpt-5.6 --level high --set max_output_tokens=64000 --yes
prx models default set openai gpt-5.6 --level high --yes
```

`--set` accepts a declared parameter name and a JSON value, or an unquoted
string such as `reasoning_effort=low`. Repeated names are refused. Settings
replace the model's saved level and explicit overrides together. Plan-time
validation refuses unknown levels, unsupported parameters, invalid types,
out-of-range values and invalid steps; it does not silently clamp a durable
choice. The loader refuses misspelled route or model-settings keys, so a field
such as `api_keyy` cannot be absorbed as a parameter. Saved values that become
invalid after a catalog update do not prevent startup or requests: runtime
drops unsupported or malformed values and unknown levels, clamps numeric
values, and warns. Load and doctor diagnostics identify settings to repair.
Explicit caller overrides retain their existing validation and precedence.

The Console Models page loads the selected model's declarations and generates
its level selector and numeric/enum controls from them. A settings change uses
the existing review-and-apply flow, revision guard and configuration history.
`GET /api/models/settings?provider=openai&model=gpt-5.6` returns declarations
and saved settings, with a `validation_error` field when a saved choice needs
repair; the read remains available even for invalid saved values.
`PUT /api/models/settings` accepts
`{"model":"openai/gpt-5.6","settings":{"level":"high"}}`; the same operation
is `{"kind":"set_model_settings",...}` on the configuration plan/apply
endpoints. `PUT /api/defaults` accepts optional `settings` atomically with the
provider and model. Authorization and root-credential ownership checks still
apply.

In terminal chat, `/model <name> [low|medium|high]` switches the model and
optional level, and `/level <name>` changes the current model's level. These
choices affect subsequent turns in that chat process. `/model save-default`
writes the current model and its settings through PMM; it can refuse a provider
change that would hand over the existing root credential. The interactive
onboarding wizard offers the catalog's three levels; both onboarding paths
write the chosen configuration through the transaction layer after safe
initialization.

Model settings belong to the configuration used to construct each provider
chain. Each route selects its own level before explicit model parameters and
explicit route parameters apply, even when two routes use the same model. Retries and streaming
requests retain these settings; actual resolved values continue to enter the
existing request preparation provenance. Revert a choice through configuration
history, or apply a new settings transaction. A binary rollback retains the
TOML file; check that the older binary understands these settings.

## Release catalog lifecycle

`prx models catalog status` reports the embedded revision, content digest,
provider/model counts and lifecycle counts. Catalog changes ship with PRX
releases. Reverting the binary restores its embedded catalog; configuration
rollback uses PMM history.

Persisted deprecated, retired or absent model references produce startup and
`prx doctor` warnings. Warnings include a published replacement when present.
Existing selections remain loadable; retired release rows supply no capability
verification. New retired selections are refused, and agent-written selections
still require verified capabilities. Custom or deleted IDs have no inferred
replacement.

`prx models migrate-retired` previews published replacements in defaults,
routes, agent bindings, fallback maps, router selections and model settings.
`--yes` applies the plan through PMM; high-risk changes also require the revision
confirmation printed by the plan. The transaction preserves provider and
credential ownership, validates settings against the replacement, and refuses
colliding settings, collapsed fallback models or ambiguous shared fallbacks. Missing replacement metadata
leaves the reference intact. Inspect the plan before applying it.

Turns retain their admitted catalog evidence and parameter revision. Authenticated
child snapshots bind both revision and exact content digest; a child binary with
a different catalog refuses the snapshot before provider construction.

Confirmed, revision-bound PMM history rollback retains retired or deleted
selections as existing invocations. Negative capability evidence and credential
ownership checks still apply; creating a new retired selection remains refused.
Retired models retain published context-window facts for compaction while their
release capabilities remain unverified. The signed terminal claim and persisted
terminal event both carry the admitted catalog revision and digest.

Catalog lifecycle notices distinguish known deprecated or retired rows from
unlisted names. Custom endpoints and wildcard-covered local model names do
not produce absence notices. Other absent catalog names are informational
at load and in doctor; deprecated and retired rows still warn.
