# Decision plane

This reference includes the 2026-10-05 H1 source addendum to the local development candidate.
Integration, behavioral acceptance, performance comparison and independent
source/service review remain pending. It is not a release-readiness statement.
See [configuration](configuration.md), [tools](tools.md),
[turn context](unified-turn-context.md), [Console](web-console.md) and
[runtime security](security.md).

## Enable, disable and rollback

This staged candidate defaults `[causal_tree].enabled` to `true`; the full init
template writes `schema_version = 1` and `enabled = true` together. Adoption and
acceptance remain pending. Remote processing still defaults to disabled, and
configured transports require source-owned estimated budget admission and actual
per-attempt accounting, as described below.

An explicit disable must retain the version marker:

```toml
[causal_tree]
schema_version = 1
enabled = false
```

Versioned false, including development snapshots, survives load, save and
`prx init --force`. Recognized unversioned legacy-default signatures become
unset and inherit the new true default; an unversioned false alongside modern
non-legacy settings remains explicit. See the [migration matrix](configuration.md#decision-plane-default-and-migration).
The loader migrates in memory; it does not rewrite files on startup.

Disabling the plane uses conservative builtin selection on migrated turn paths;
it does not grant the full tool registry.
It disables remote decision selection and decision/outcome telemetry writes.
Ordinary conversation recording and terminal persistence retain their own
configuration. An existing decision database remains inspectable without
creating a new database. Complete ingress parity still requires validation.

Configuration changes use the management transaction flow. For example,
`prx causal config disable` previews the rollback plan without writing it.
Applying requires `--yes`; a high-risk plan additionally requires
`--confirm-high-risk <REVISION>` naming its reported revision. Keep the prior
configuration revision and evidence before an authorized change. Do not erase
databases, cost journals or pending approvals as a rollback procedure.
Disabling the plane does not cancel a dispatched request or refund its cost.
Causal configuration changes remain restart-required: a desired configuration
revision is not proof that the running generation has changed. This source
cutover does not restart a service or modify an existing configuration file.

## Admission and backend selection

Migrated turns bind the original input, owner, session, configuration generation
and actual provider selection before execution. Candidate hard gates constrain
capabilities, inherited child scope, cost and depth. Retrieve/Discover may
produce a bounded next decision hop; selected context and tools pass through
execution binding and the final authority checks. A terminal outcome joins the
decision after terminal persistence. A delegation launch acknowledgement is not
proof that its child completed.

With the default builtin backend, a nonempty conversational turn with no
structural or exact capability target can proceed to the serving model when its
best admitted candidate is a direct answer, without requiring a retrieval
artifact. This applies to the first message and later ordinary conversation.
An earlier failed provider attempt is not an unsuccessful action step, so it
does not make a later ordinary message require external evidence. Empty input
and turns made undecidable by failed or repeated tool work still Clarify. Brief
ordinary requests (builtin request weight at most two), continued conversation,
and turns with no viable read/recovery candidate receive the direct-answer
preference. Longer unrouted text may take one bounded discovery hop. After that
gate-checked local read completes, the terminal fallback may answer even when
the source contains a path, URL or code shape. An exact capability mention or
a pending approval remains nonterminal after discovery. Any capability found
by the read still needs a normal nonterminal selection and all execution gates;
completion of discovery grants no execution authority. Configured decision
attempts all rejected for semantic validation remain fail
closed even when the builtin shadow completes discovery. Transport, 5xx,
circuit-open and local decision-budget failures fall back to the builtin; a
later configured attempt that serves also keeps the normal terminal path.

An exactly named, current Searchable tool, or an exactly named source-configured
Delegate with a sealed child manifest, uses a narrower selection path. It may
commit without an external artifact, the evidence-sufficiency abstention, the
choice-confidence check, the side-effect-scaled commit threshold, or the
minimum top-candidate margin. It still must be a single exact current reference
in the catalog generation and pass surface visibility, exclusions, model
allowlists, readiness, freshness, source authority, and side-effect-mode gates.
An approval-required tool remains pending in Supervised mode, and execution
still requires its exact approval/grant and idempotency authority. Delegation
also retains the source's delegation authority and sealed child-capability
boundary.

An Answer turn may publish the bounded Direct core plus `capability_search` when
the source/model may call tools and an eligible discovery family exists. A brief
answer with no structural target, such as a greeting, keeps only the Direct core;
search becomes available when the input has a URL, path or code shape, or exceeds
the builtin brief-request bound. If the runtime has no current
`capability_search` backend, it omits that control instead of failing the Answer.
An Answer that follows a successful selected Memory, Document or Session read
consumes the admitted evidence and keeps the bounded Direct core, so the serving
model can perform a subsequent ordinary operation. The read does not reopen
Searchable tools or discovery. An Answer turn may also publish an exactly named
`delegate` or `sessions_spawn` on any surface that admits delegation, even when
that delegator is Searchable rather than configured as Direct, but only when the
same decision carries a sealed child manifest.
Interactive Chat additionally places `sessions_spawn` in its Direct core.
Exclusions, tools-off, read-only and plan interception still remove entries, and
publication alone never executes one. Channel and gateway turns use these shared
rules; they do not need a separate direct-surface configuration. Every actual
call is rechecked against source, schema, approval and execution policy.

Conversation history used by the provider and the boolean used by admission are
separate contracts. The admission input is currently populated as follows:

| Entrypoint | Admission history input |
| --- | --- |
| Interactive Chat | True after the in-process transcript contains a non-system turn |
| Gateway webhook and authenticated Console | True when the owner/session-scoped durable history contains an earlier user or assistant turn |
| Single-shot/REPL Agent CLI, channels, cron, Xin, delegate and session workers | False; these sources admit the raw current input even when their provider request or worker state carries earlier context |

The effective model window is pinned before decision dispatch. After the
configured response reserve, the runtime reserves one sixteenth of the remaining
window for the sealed decision request, capped at 4,096 tokens and by
`causal_tree.policy.extra_token_ratio_limit` times that baseline window. The
decision and main-turn input slices must both be positive. A smaller effective
window, or a configured ratio too small to fund one decision token, fails closed with
`resolved Agent context window has no safe decision reserve` before provider
execution. Published tool schemas and their prompt instructions are then
reserved from the main-turn slice as part of each provider preflight.

Use `prx causal backends` to inspect the computed backend order and exclusion
reasons; `prx causal status` also reports health and circuit state. Availability
or configuration alone does not authorize a physical send. The builtin is the
local fallback unless every configured attempt failed semantic response
validation; that condition terminates the decision instead of silently changing
its meaning. Transport, server, open-circuit and decision-budget failures use the
builtin fallback. Configured physical attempts require source-owned accounting:
the runtime first applies a conservative byte/token upper bound and otherwise
estimates the guarded request with its existing token estimator, reserves the
remaining declared token allowance at the highest applicable PMM rate, and
checks the original token, latency and monetary limits before dispatch.
The estimate is not a vendor-enforced billing cap. Each retry has its own durable
reservation; builtin and cache-only paths do not open the cost ledger.

Fully reported input/output usage replaces the estimate using pinned PMM prices.
Actual usage above the estimate is recorded in full. Missing, partial or estimated
usage retains the pending token and money liability; cancellation does not turn
an unknown charge into zero. Local budget refusal does not damage backend health.
Post-prediction gates and scoring use remaining budgets without rewriting the
fingerprinted request state. A priced direct Answer has zero extra execution
latency because the serving-model response is the turn's baseline path; its
token and monetary estimate still pass G6. Retrieval and tool candidates keep
their own execution-latency estimates. Administrative probes/tests use a
separate owned scope under the existing live-test permission checks. An
unfunded probe is reported as unprobed, not as successful health evidence.

Supported auxiliary transports account for each actual send, including nested
fallbacks. Unsupported transports refuse the additional call before dispatch.
The candidate still requires assembled behavioral, cancellation and fixed-budget
performance validation; these source contracts are not release acceptance.

### Scoped tool-call observations

Tool execution keeps its existing source/session-scoped idempotency key: a cached
terminal returns the original result without another invocation, an unresolved
reservation refuses a new invocation, and a proven prestart abandonment can
continue. A fresh call ID is not prohibited merely because its arguments match
an earlier call.

New optional `exact_call_observations` record bounded keyed identities and distinguish
actual backend failures from cached returns and unsent continuations. The real
execution trace carries these facts into the signed terminal and confirmed serial
segment observation. Missing legacy facts remain absent; dropped or overflowing
observations are explicitly incomplete. The new strict reader rejects unsupported
or inconsistent facts. Older strict readers cannot consume newly populated facts;
absence retains the previous encoding and MAC input.

G8 mirrors an actual retry policy when one exists. The generic runtime has no
configured tool-call retry limit, represented explicitly as absent; provider,
scheduler and node-transport retry settings are not a global tool retry rule.
The existing failed-candidate namespace projection retains its old meaning.
These observations do not issue permission, infer an intentional retry across new
call IDs, or authorize replay after restart.

## Intentional silence and terminal evidence

On an ingress that explicitly exposes `stay_silent`, an actual provider response
may end the turn without an assistant message. The candidate binds this outcome
to the admitted source, current tool descriptor and completed provider trace.
It retains reported usage and settles a distinct `silent` outcome with partial
completeness and no executed action or target. Silence is not a delivered answer
and cannot authorize Router answer training.

A raw terminal status does not establish intentional silence. Missing or
inconsistent execution facts are rejected before terminal or cost writes in
both recorded and disabled decision modes. Authenticated replay retains this
check; a marked silent execution cannot be relabeled as a completed answer.
Failure and cancellation keep their existing precedence. Ordinary older traces
without the marker default to unconfirmed silence; ordinary serialization omits
a false marker. Older strict readers may reject new signed silent records, so
retain the originating revision and evidence for rollback instead of rewriting
those records. Runtime and performance acceptance of this candidate is pending.

## Endpoints and credentials

Backend kinds are `builtin`, `compatible_http`, `typesafe` and `openrouter`.
Remote processing requires explicit opt-in through `remote_enabled`; hosted
backends remain remote even when an endpoint override is supplied. OpenRouter
requires an exact pinned snapshot. Hosted snapshot values must satisfy the
configuration validator rather than name a floating alias.

For a hosted model present in the embedded release catalog, store a provider
owned credential and select the exact catalog model through the managed flow:

```bash
prx providers credential set openrouter --name decision-openrouter --from-stdin
prx causal config set backend --kind openrouter --model typesafe/jev-1.13 \
  --reference cred-decision-openrouter --apply
prx causal config enable-remote --apply
```

The backend transaction fills the catalog's pinned snapshot, transport timeout,
latency budget, and built-in price. A model absent from the release catalog
still needs an explicit pinned snapshot, timeout, and `[cost.prices]` override.
Use the plan's revision and the normal high-risk confirmation flags when the
command asks for them. `prx causal status`, `prx causal test`, and `prx doctor`
show the resulting configured values before a live test is attempted.

Plain HTTP is permitted only for accepted loopback spelling: canonical decimal
127/8 addresses accepted by the validator, `[::1]`, or `localhost`
(case-insensitive, transport-pinned to `127.0.0.1` without DNS). Abbreviated or
numeric alternate IP forms, mapped IPv6, `localhost.` and subdomains are not
loopback exemptions. Other endpoints require HTTPS. URL user information,
query strings and fragments are refused.

`credential_ref` names a stored credential; it is not a raw key. Use the
credential management flow and protected input, never URL parameters, logs or
shell command arguments containing a secret. Backend changes are available
under `prx causal config set backend`; remote opt-in uses
`prx causal config enable-remote` with the management confirmation flow.

## Inspection and management interfaces

| CLI | Purpose |
| --- | --- |
| `prx causal status` | Effective status, exclusions and store information |
| `prx causal backends` | Backend ordering, health and exclusions |
| `prx causal test` | Neutral fixture; builtin/loopback tests may execute locally |
| `prx causal test --live --yes` | Explicit remote test; may incur a charge |
| `prx causal inspect --decision-id <ID>` | Recorded decision and outcome join |
| `prx causal evaluate --from <DATE> --to <DATE>` | Evaluate stored records and save a report |
| `prx causal config apply-calibration --report <ID>` | Preview a report-derived policy change |

Evaluation does not apply configuration. Calibration is tied to the configured
backend identity and pin. Predictions are not causal proof: correlations and
scores in a report do not establish that a chosen action caused an outcome.

Authenticated Console API routes are mounted under `/api`:

| Method | Path |
| --- | --- |
| GET | `/causal/status`, `/causal/backends` |
| POST | `/causal/test` |
| GET | `/causal/decisions`, `/causal/decisions/{id}` |
| GET | `/causal/reports`, `/causal/reports/saved` |
| POST | `/causal/config/plans`, `/causal/config/apply` |
| PUT | `/causal/credential` |
| GET, POST, DELETE | `/sessions/{id}/approvals/{decision_id}` |
| POST | `/sessions/{id}/approvals/{decision_id}/resume` |

Inspection does not grant execution authority. Approval review, revocation and
resume retain authenticated owner/session scope, expiry, exact operation
binding and one-use consumption. Channels without an approval surface remain
pending or refuse; ordinary channel text must not mint a grant.

## Store, retention and compatibility

Telemetry lives at `<workspace>/self/causal_tree/decisions.db`, separately from
agent memory. Its bounded asynchronous writer can drop telemetry under pressure;
a missing telemetry row is not proof that no execution occurred. Durable source,
approval, terminal and cost evidence have separate authority and recovery roles.
Reads are scoped; unknown and out-of-scope decisions share the same response.

`store.retention_days` defaults to zero, meaning follow fitness retention.
An effective zero-day retention horizon keeps records indefinitely. The maximum
configured value is 3650 days. `record_redacted_projection` defaults to `false`.
Retention deletes old decisions and their joined outcomes; preserve required
acceptance evidence independently before an authorized retention change.

Imported v1 records are marked legacy and do not acquire invented probability
distributions. Ownerless imported rows are local-operator-only. Compatibility
readers remain in the candidate. No removal date or release window has been
accepted; retain compatibility until that decision and its migration validation
are complete. A newer database schema must not be opened
for writing by an older binary. Do not force a downgrade by deleting metadata.

## Interruption recovery in the local candidate

For enabled, recorded inputs on SQLite, PostgreSQL and the SQLite-backed Lucid
adapter, the candidate commits the original message and authenticated recovery
metadata together. Disabled mode and disabled recording do not create this
metadata. An entrypoint that needs a durable source still refuses execution when
recording is unavailable. Other memory backends must explicitly support these
recovery operations; they do not inherit a synthetic recovery guarantee.

Recovery retains the actual backend lifetime and the original workspace, owner
and source. A duplicate input with the same stable identity retains its original
receipt; it does not receive a new execution authority. Missing final evidence
remains interrupted/unconfirmed. Recovery never retries a provider or tool to
find out whether the earlier attempt succeeded. Historical inputs without
recovery metadata are not backfilled with new authority.

The recovery reader processes bounded pages of at most 64 records, with a 16 KiB
capsule limit, a 1 MiB event limit and bounded identity fields. Oversized input
that cannot satisfy the durable-source contract is refused before dispatch;
these bounds are not permission to truncate the original message. A late final
must pass authenticated source and terminal replay before outcome, applied-state
or Console projection repair is acknowledged.

A separate retained-decision scan repairs missing parent/child telemetry from a
verified signed child final. It checks original parent and child lineage and
reads back the exact outcome and link. It grants no child execution authority
and does not turn an unconfirmed ingress receipt into a reconciled one. Missing,
corrupt or expired evidence stays unconfirmed. The cursor wraps to revisit late
commits; retention or loss of the original decision chain prevents a complete
recovery claim. Process, cancellation, partition and retention scenarios remain
pending concentrated validation and independent review.

Delegated worker recovery distinguishes the logical conversation workspace, the
decision store's persistent namespace and the worker's private execution
directory. The actual store namespace must match the original signed binding;
execution policy must match the authenticated worker manifest. New delegated
launch receipts use version 2 and model-delegated batch receipts use version 3
to retain that original namespace. Ordinary batch version 2 is unchanged.
Historical receipts without the namespace remain readable and signature-checkable
but unconfirmed for this repair path; recovery does not guess that the currently
opened database is the original one. Existing records are not rewritten. Older
binaries may refuse the new receipt versions; preserve the records and restore
the matching candidate for their inspection or reconciliation.

## Operational checks and threat boundaries

Start with status, backend exclusions and scoped inspection. For a refused
turn, distinguish capability denial, pending approval, stale generation,
unproven budget and unavailable telemetry before attempting recovery. Resume
through the authenticated approval endpoint; do not replay a consumed grant or
manually edit its record. Preserve unresolved dispatched cost reservations
across restart until actual usage is reconciled.

The relevant threats include forged source identity, stale or widened tool
selection, cross-owner reads, approval replay, endpoint/credential leakage and
unbounded paid retries. Local source checks do not establish protection on every
ingress. Full boundary tests, fresh-process recovery, browser acceptance and
independent reviews remain mandatory. Performance acceptance retains the
8278 allocation ceiling and zero catalog-growth tolerance; no current document
or structural compile result waives those limits.

## Selected document retrieval in the candidate

The candidate now offers a Document Retrieve only for the exact read-only
`document_search` catalog entry. Selected document reads use a bounded local
SQLite FTS port, including through Lucid, without embedding generation or cache
writes. Other backends fail closed unless they implement that port. Returned
summaries remain Document artifacts; embedded memory presentation callbacks
receive only Memory summaries. Catalog, source, exclusions, deadline and live
generation checks still apply. Full behavioral/performance acceptance remains
pending.

## Selected session history in the candidate

A Session Retrieve requires an exact read-only `sessions_history` catalog entry
and a durable current source event. SQLite verifies that event's owner,
workspace and canonical session, then reads a bounded chronological history
strictly before it. Current and later messages are excluded; existing visibility
and legacy-session predicates govern historical rows. Lucid forwards locally;
unsupported backends fail closed. Session summaries retain speaker labels and
remain separate from Memory callbacks. Candidate and service wiring is present;
full source parity, legacy session propagation, security and performance
acceptance remain pending.

Selected document/session reads use the source-owned principal, including
agent/persona and a legacy-session alias where the direct ingress carries one.
A child source instead uses its issued child scope or verified input receipt;
it retains the inherited owner-isolation setting but does not inherit the
parent session's legacy alias. The service rechecks workspace, owner, session
and isolation before accessing the backend. Full child-process and cross-owner
behavioral validation remains pending.

CLI, Chat, Gateway/Console, Channel and child turns now share selected-context
rendering with the embedded Agent. Memory-only text keeps its existing format;
Document and Session summaries receive explicit type labels. Discovery entries
are rejected as prompt evidence by this renderer. This wiring does not prove
entrypoint parity or change the required fixed performance budgets.

### Candidate memory-backend support

Durable event storage and selected evidence retrieval are separate contracts.
A backend that records `message_events` is not thereby able to perform bounded
Retrieve or rehearsal operations.

| Backend | Memory / Document / Session Retrieve | Read-only rehearsal |
| --- | --- | --- |
| SQLite | Implemented; behavioral and performance acceptance pending | Implemented; acceptance pending |
| Lucid | Delegates to its local SQLite store only | Delegates to local SQLite only |
| PostgreSQL | Dedicated read-only ports implemented; real-server, cancellation and performance acceptance pending | Dedicated port implemented; acceptance pending |
| Markdown / None | Unsupported; explicit error | Unsupported; explicit error |
| WASM storage adapters | Unsupported; explicit error | Unsupported; explicit error |

PostgreSQL now implements the four decision-read ports through a dedicated async
connection and read-only repeatable-read transaction. Queries carry scoped
principal/owner predicates and refresh statement and lock timeouts against the
remaining deadline. Session reads verify the source event and exclude current
and later events. This path does not use the ordinary pool or generate document
embeddings. Connection cancellation, server-side work termination, ACL parity
and fixed performance budgets still require real-server acceptance; the presence
of these ports is implementation evidence only. Ordinary pool behavior is unchanged.

Unsupported ports do not fall back to semantic recall, remote Lucid operations,
or unscoped reads. A selected unsupported operation fails explicitly; operators
must not interpret successful startup or durable event recording as retrieval
compatibility. This candidate does not change a configured backend automatically.

The legacy speculative CTE execution modules and JSONL feedback writers have
been removed from this candidate. The `v1_compat` decoder/importer remains for
historical files. Importing old feedback does not run the old engine or resume
an old approval. The derived legacy configuration fields have also been removed; historical
configuration keys still pass through the existing migration and rejection rules.
Consumers read canonical fields directly, and decision side effects remain
read-only. The final migration/default gates remain pending; removal alone is not release acceptance.

The decision domain now uses `CausalState` for source-owned trusted, observed,
predicted and derived facts, and `CostEstimate` for bounded token/micro-USD
estimates. The old history-derived state, speculative branch and path-commit
types are removed. These are internal type changes; protocol codes, stored
record fields and the v1 decoder have not been renamed. Feature-matrix runtime
and migration verification remain pending.

The retired `CteRun`/`CteExtraLatency` observer interfaces and their Prometheus
(`prx_cte_*`) and OpenTelemetry (`prx.cte.*`) instruments are removed. Current
decision-plane events and bounded metrics remain. Historical dashboards must
use the causal decision metrics; old and new latency series are not interchangeable
performance evidence. Configuration-only control-ladder traces point to
`causal.directive.applied` events; successful ingress and child publications also emit
`runtime.control_ladder.decision_published` from the exact service status when
INFO tracing is enabled. Its `published` status includes configuration/catalog
generation, store mode, recordability and bounded backend state. It does not
prove backend admission, a completed turn or directive execution. The startup
configuration snapshot remains separate and is independent of `llm-router`.

### Decision metrics and events

Decision-plane events always flow to the configured `[observability]` backend
(`log`, `otel` or `prometheus`). `[causal_tree.observability]` (`decision_log`,
`metrics`) and the earlier `write_decision_log` / `write_metrics` never had a
runtime effect and are gone; an existing file that carries them still loads,
the keys are dropped and the load logs which. To stop the events, choose a
different `[observability]` backend.

The Prometheus backend implements these seven decision metrics:

| Metric | Type | Labels |
|---|---|---|
| `prx_causal_decisions_total` | counter | `backend`, `locality`, `served_by` |
| `prx_causal_prediction_failures_total` | counter | `backend`, `error_class` |
| `prx_causal_fallbacks_total` | counter | `from`, `to` |
| `prx_causal_circuit_transitions_total` | counter | `backend`, `state` |
| `prx_causal_decision_latency_seconds` | histogram | `backend` |
| `prx_causal_outcome_delay_seconds` | histogram | none |
| `prx_causal_store_dropped_total` | counter | none |

This is seven of the fourteen metrics the decision-plane specification names;
the other seven are not implemented, so a dashboard must not expect them.
Candidate generation, gating, scoring and commit events are still emitted as
events (logs and OpenTelemetry spans) but do not increment a Prometheus series.

`[causal_tree.scoring] version`, when set, is checked against the scorer
(`scorer-v1`): a different value refuses the configuration instead of applying
weights chosen for other scoring. A calibration applied with
`prx causal config apply-calibration` writes its thresholds into
`[causal_tree.policy]`, which selection reads, and records the backend it was
measured on in `calibration_profile`; validation refuses any configuration
whose backend kind, endpoint or model no longer matches it.

### Selection thresholds

These values are fixed constants, not configuration:

| Constant | Value | Meaning |
|---|---|---|
| Evidence threshold | 0.5, inclusive | `evidence_sufficient` at or above it is sufficient to answer; below it the selector retrieves, discovers or abstains. A plain conversational turn is reported by the builtin model at exactly 0.5 (`PLAIN_TURN_EVIDENCE`), so it answers directly |
| Freshness threshold | 0.5, exclusive | `needs_fresh_state` above it marks observed artifacts stale and prefers a retrieve |
| Deterministic propensity | 1.0 | recorded for every non-exploratory selection; only read-only exploration replaces it |
| Approval lifetime | at most 5 minutes | an approval request whose expiry is later than five minutes after issue is refused |

## Terminal authority and logical task completion

Enabled and disabled model turns finalize through `TurnTerminalCommit`, whose
`decision` field is required. Callers assemble `TurnTerminalDraft`; its optional
field permits the owning guard to attach service-issued authority before sealing.
Converting an unbound draft fails before terminal writes. The draft API does not
provide an unbound finalization path.

The disabled link checks source, history scope, workspace, memory instance and
cost configuration, but contains no decision-store writer, reader or replay key.
It creates no decision claim or causal outcome. Ordinary usage and history
persistence remain separate. Chat replay checks the link's durable recording
authority rather than merely testing whether a link is present.

Logical task completion uses separate authenticated authority and does not
settle a model call again. In-process task authority is issued from a verified
parent grant/input receipt, ordinary task authority or operator task authority
before registration. Process workers likewise bind their authenticated source
before the driver runs. Logical completion retains memory and scope, rejects
model outcomes and decision links, and binds retry history and marker payloads.
Successful process publication additionally requires recorded and confirmed
segment counts to match. These paths grant no provider or tool execution.

Delegate model calls enter admitted child turns, including non-agentic children
with tool execution constrained by configuration. The old direct-provider branch
and duplicate terminal finalizer are removed. Failures before child decision
issuance retain task failure events without invented model usage or model terminals.
Runtime, cross-process recovery and fixture acceptance remain pending; these
implemented boundaries do not make the candidate ready for adoption.

## Embedded MemoryLoader compatibility

An embedded custom `MemoryLoader` requires `runtime_config` and runs only for
selected Memory retrieval. Its callback receives a restricted `Memory` facade,
not the original backend. Only bounded `recall` is supported: queries are at most
1024 bytes, each limit is 1–5, at most five recall calls may reserve a total of
five hits, and any supplied session must match the admitted session. The source
workspace, owner and decision deadline remain authoritative.

`get`, list/count, context-aware recall variants, writes and conversation/profile
APIs are refused and require callback migration; accepting `&dyn Memory` does
not preserve unrestricted backend access. Returned context must fit 1280
characters, 5120 bytes and 2048 tokens, survive redaction within those bounds,
and have actual retrieved Memory hits when nonempty. Extension text is prompt
presentation, not retrieval or execution authority. Document/Session artifacts
use their own typed presentation. `SelectedMemoryFormatter` receives already
selected summaries only. Runtime and performance validation remain pending.

Chat ingress derives the event owner from the actual memory-fabric workspace.
Request events retain the verified ingress envelope, including its configuration
generation and source-event causation. A failure before a decision is issued uses
`turn.admission.failed`, separately from model termination and cost settlement.
When user-message recording is disabled and no durable source exists, that input
closes locally without claiming a persisted failure event. Admission failure and
decision issuance are serialized for the same input. These candidate paths still
require the full behavioral, cancellation, privacy and performance gates; existing
historical events are not rewritten by this change.
