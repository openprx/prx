# LLM Router

The original Router introduced heuristic ranking, semantic history and Automix
policy. The candidate contract below supersedes its earlier unrestricted
history and premium-escalation behavior. Candidate assembly and concentrated
runtime/performance validation are still in progress; this is not release acceptance.

## Source addendum (2026-10-05)

This addendum supersedes the paid-call refusal statements in the preserved
October 4 candidate snapshot below. It describes source implementation, not
compiled, exercised, deployed or performance-accepted behavior.

Supported configured paid embeddings can consume the actual selected Router
history grant through a matching transport-accounting scope. LocalHash still
requires the concrete local instance; arbitrary dynamic embedder names grant
nothing. The existing history deadline, domain, capacity and model readmission
rules remain unchanged. Every actual paid attempt uses pinned PMM pricing and
an owned estimated reservation; missing support, scope, pricing or budget refuses.

Automix rules can request one premium attempt within a separate bounded
escalation window, starting when the cheap reply returns.
`router.automix.escalation_window_ms` defaults to 30,000 ms and must be between
1 and 120,000 ms. Main settlement, premium reservation and premium transport
all consume this same window; a retry cannot restart it. The original token
and money balance and settlement gate remain shared. The decision budget
remains 500 ms and the Router budget remains 100 ms. The actual registered provider instance is
preserved. Known cheap usage is settled under its unchanged terminal ID before
premium admission; partial or unknown main usage cannot be treated as zero to
fund escalation. Premium physical usage settles separately. A failed or unknown
premium call retains the cheap result and its trace. A successful premium reply
is bound to its actual auxiliary observation, and recorded replay authenticates
that event and reply projection. Metrics/history use the actual served model
within the configured Router learning policy; no unknown-model priors are added.

Estimated reservations are not vendor-enforced charge caps. Actual reported
usage above an estimate is recorded in full; unresolved usage retains liability.
The 8278 allocation limit, zero catalog-growth tolerance and all original token,
latency and cost budgets still require centralized verification without relaxation.

## Candidate contract (2026-10-04)

Historical snapshot: paid-call refusal descriptions in this section are
superseded by the October 5 source addendum; its earlier status is not rewritten
as successful runtime evidence.

The embedded Agent consumes the source-bound Router decision. Local KNN uses
one purpose-bound Memory Retrieve in the original decision chain before model
readmission. It shares the original hop limit and deadline; its history read,
local embedding and ranking also share the existing 100 ms query allowance.
A failed selected read closes through the decision chain without authorizing
a model call. Ordinary Memory retrieval and custom `MemoryLoader` callbacks
do not authorize Router history reads.

When decision recording is enabled, the selected history hop is durably written
and read back before history I/O. A dropped caller can therefore leave an
inspectable pending selection; that row alone does not prove a completed read
or model turn. Disabled or privacy modes retain their no-decision-write contract
and do not promise this durable observation. Router history observations are
separate from query evidence and cannot justify a factual answer.

History is isolated by actual backend, workspace, owner, session, policy,
embedding model, dimensions and encoding version. SQLite, PostgreSQL and
Lucid's authoritative local SQLite store implement the scoped ports. Reads
include the complete eligible domain before the current source sequence;
they reject more than 1024 records instead of selecting a recent subset.
Vectors are limited to 2048 dimensions. The selected-read grant retains its
1024-byte query limit; the 64 KiB storage envelope does not enlarge that grant.
Malformed or oversized history fails explicitly. Other backends must implement these
ports; generic recall is not a fallback.

Local embedding authority retains the concrete `LocalHashEmbedding` instance.
A dynamic embedder's name is not evidence of local or uncharged execution.
The library attachment API is `RouterEngine::with_local_hash_history`; it
must run before the engine is bound to an Agent source. Paid embeddings and
Automix premium calls require a proven transport cost contract and currently
remain `BudgetUnproven`. A low-confidence cheap result is retained with its
actual trace; configuration alone does not trigger a premium request.

Training uses the actual durable terminal receipt and served provider/model,
with the query vector retained from the admitted history phase. It does not
embed the query again or import legacy global history. Receipts remain in the
bounded domain to prevent duplicate learning; capacity exhaustion rejects new
records instead of evicting receipts. Owned completion tasks retain backend
writes through cancellation. Unknown commit results prevent successful cleanup
and further admission on that Agent instance. Current terminal training does
not establish complete attribution of every failed physical attempt.
The revised actual-outcome learning policy uses a new domain version; earlier
statistics based on reply text remain historical data and are not imported.

The embedded Agent currently creates a new session identity for a new instance.
Persisted rows do not imply statistics automatically resume across a new Agent
or process. Existing legacy global records are not a source-authorized migration.
New Router terminal projections are readable by the new historical reader;
older strict readers may reject them. Ordinary receipts without the optional
projection keep their previous serialization. Historical replay never creates
a live read grant or model-execution authority.

## Feature Switches

- Build-time: `llm-router` feature is enabled by default (`cargo build`).
- Local heuristic ranking requires an admitted source and reachable model binding.
- KNN requires the scoped history and concrete embedding authority described above.
- Premium escalation requires the owned estimated admission described in the October 5 addendum.

## End-to-End Flow

Text flow (runtime path):

`source admission -> selected history read (when configured) -> PMM model readmission -> decision -> provider execution -> durable terminal -> scoped learning`

Step details:

1. `intent_classify`
   Classify message intent (`conversation`, `analysis`, `code`, etc.).
1. `select_model`
   Router computes candidate score:
   `alpha*similarity + beta*capability + gamma*elo - delta*cost - epsilon*latency`.
1. `reliability_fallback`
   If the selected model/provider fails, standard reliability fallback applies.
1. `automix`
   Low confidence can record a `BudgetUnproven` refusal; retain the cheap result
   without making a premium request.
1. `record_outcome`
   Use the real durable terminal and actual serving member for scoped metrics
   and the retained-vector history write. Nonempty reply text is not success evidence.

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
alpha = 0.0
beta = 0.5
gamma = 0.3
delta = 0.1
epsilon = 0.1
knn_min_records = 10
knn_k = 7

[router.automix]
confidence_threshold = 0.70
cheap_model_tiers = ["cheap", "fast", "mini"]
premium_model_id = "openai/gpt-4.1"
escalation_window_ms = 30000

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
knn_min_records = 10

[[router.models]]
model_id = "gpt-4o-mini"
provider = "openrouter"
categories = ["conversation"]
```

Operational note:

- Ensure provider credentials and network are valid.
- Verify health with `prx channel doctor` and your deployment health checks
  before enabling router in production.

## Field Reference

`[router]`

- `alpha`: semantic similarity weight (KNN score)
- `beta`: capability/category weight
- `gamma`: Elo weight
- `delta`: cost penalty coefficient
- `epsilon`: latency penalty coefficient
- `knn_min_records`: minimum domain history records before KNN contributes; only successful records vote
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

- `confidence_threshold`: escalate when confidence `< threshold`
- `cheap_model_tiers`: markers used to classify cheap-first targets
- `premium_model_id`: escalation target model id (`provider/model` supported)
- `escalation_window_ms`: premium window after the cheap reply, including main
  settlement and physical reservation; default `30000`, hard maximum `120000`,
  minimum `1`. Omitting this field in existing configuration uses the default.

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
- Route capability truth:
  the explicit route owner reports capabilities for the resolved provider and
  request mode. A reliability chain reports the safe intersection across all
  compatible candidates.
- Credential isolation:
  each routed provider resolves its own credential through the shared provider
  resolver. Credentials supplied for the primary provider are not copied into
  another provider's route.
- Attempt attribution:
  both non-streaming and streaming fallback retain ordered attempts, including
  failures before content. The successful attempt determines the final
  provider/model recorded at the terminal boundary.

## Operations Guidance

- KNN contributes after the admitted domain reaches `knn_min_records` and a concrete local embedder is bound.
- Keep `knn_min_records >= 10` to avoid unstable early routing.
- Configured confidence thresholds do not bypass the premium-request cost gate.

### PostgreSQL learning namespace — 2026-10-05 compatibility note

Bounded Router learning state and terminal-receipt tables now use a private namespace derived from the full SHA-256 digest of the configured, qualified message-event relation, matching the existing Router-history isolation rule. Two memory base tables in one PostgreSQL schema do not share learned snapshots or receipt identities. Existing schema-wide `router_learning_state` / `router_learning_receipts` rows are left untouched and are not imported automatically because they do not identify their originating base table; the private scope starts from configured priors until new confirmed outcomes are recorded. This change does not perform a live database migration or establish PostgreSQL service or performance acceptance.
