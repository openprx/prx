# Provider routing and cost lifecycle

PRX resolves provider credentials, capabilities, execution attempts, token
usage, and cost settlement through explicit shared boundaries. This prevents
entrypoints from deriving different answers for the same turn.

## Before execution

Provider construction and availability inspection call one credential resolver.
It combines an explicit override with the provider's own configured or OAuth
context. Routed construction permits the primary credential only for the named
primary provider; every different route must resolve its own credential.

Capabilities are queried with the requested model and mode (`non_streaming` or
`streaming`). A direct route delegates to its resolved provider. A reliability
chain exposes the intersection across compatible failover candidates. This is a
safe admission answer: a request is not accepted with tools or vision when a
candidate that may execute it cannot honor that capability.

## During execution

Every provider/model attempt receives an ordered trace record. Streaming
fallback emits metadata-only chunks for attempts that fail before content and
attaches the successful attempt to the final stream boundary. These metadata
chunks do not become assistant text. The agent uses the successful trace record,
not the originally requested provider, for final attribution.

## Terminal settlement

The shared model-turn terminal commit requires service-issued decision authority.
A `TurnTerminalDraft` may omit it while the owning guard prepares the commit;
conversion to `TurnTerminalCommit` refuses a missing link before persistence.
Logical task/worker completion has a separate authenticated path and must not
settle provider usage for a second time.

For a metered model outcome, the terminal path:

1. derives the metered record with the terminal id as settlement id;
2. writes the idempotent canonical `usage.settled` event;
3. projects priced usage into the workspace `CostTracker`;
4. records the applicable assistant history and provider outcome events;
5. writes `turn.finalized` last, including usage and cost-settlement results.

Priced projection is durable and idempotent by settlement id. A matching replay
returns `replayed`; conflicting usage is refused. Missing pricing returns
`unknown_pricing`, never a zero-cost substitute. A catalog row marked
`billing = "subscription"` has no token amount. A turn and a decision attempt
both return `subscription` with the provider, model, and token counts, and
neither appends a dollar row. An operator `[cost.prices]` override still
prices that model as a token charge. A subscription decision attempt
reconciles its reserved hold at zero micro-USD without a priced cost row.
Canonical incurred usage is projected even when optional tracking
is disabled; disabling reporting does not erase a charge. Projection failure
remains an explicit terminal settlement result and is not evidence that no
provider cost was incurred.

Physical subscription settlements use versioned `subscription_settled_v1`
reservation-journal entries. Existing journals remain readable. After such a
settlement, a binary without that event type cannot replay the journal; rollback
must retain a compatible reader and preserve the accounting journal.

On Unix the cost ledger and lock reject symlinks, shared hard links, non-regular
files and files/directories writable by other accounts. Appends sync the ledger
and its directory. Legacy migration publishes a fully copied, synced temporary
file under the ledger lock and preserves the original. Interrupted temporary
files are not balances. Non-Unix recording retains native locking and file sync;
it does not claim the Unix directory-handle guarantees. These source changes
still need filesystem, concurrency and recovery validation.

## Prospective budget boundary

The 2026-10-05 source addendum implements estimated admission for configured
decision attempts and supported auxiliary transports. The actual admitted source
owns the original deadline and shared token/money allowance. Each physical send,
including a permitted retry or nested supported fallback, reserves separately
at pinned PMM prices. Administrative calls use a separate operator-owned scope.
Builtin and cache-only paths do not create an accounting owner or open a ledger.

An estimate is not a vendor billing cap. Fully reported usage is settled before
semantic response acceptance, at the full actual token count and price. Missing,
partial or estimated usage keeps the hold, including across timeout and recovery;
the first hold is never refunded merely to fund a retry. Compatible loopback
decisions and health probes allocate the remaining token estimate equally across
the transport-owned remaining sends (two initially, one on retry), under the
same aggregate token/money ceiling. Each tranche must pass the full-body input
preflight. Remote and auxiliary attempts reserve the full remaining allowance.
The versioned local estimate does not impose a vendor output-token cap.
Remote decision retries remain prohibited. Unsupported transports, missing owned
scope, unknown pricing and insufficient budget remain explicit refusal cases.
These source changes still require assembled lifecycle and performance validation.

## Embedded local routing

The 2026-10-04 local candidate, with `llm-router`, runs an injected local ranker
after durably recording and authenticating input. It checks the pinned Router
policy, model routes, provider and memory instance. For KNN, the configured local
factory retains an actual `Arc<LocalHashEmbedding>`; embedded callers may attach
that concrete instance with `RouterEngine::with_local_hash_history` before
passing the engine to `AgentBuilder::router`. Arbitrary dynamic embedders do not
receive this authority from their name or model label.

History access requires the actual decision's selected Retrieve purpose grant.
The read checks the original durable input and full history domain, then bounded
local embedding and ranking can select a PMM provider/model connection. A new
linked decision admits that selection before provider execution. Read, embedding,
ranking or readmission failure produces a same-model fixed close with the real
read hop and its dependent closing decision; it does not invent a successful
PMM binding or retry the history read. A successful selection may still end in a
non-provider directive such as clarification or approval.

SQLite and PostgreSQL implement the bounded history ports; Lucid delegates them
to its actual local SQLite backend. Other backends explicitly refuse unsupported
ports. The domain binds workspace, owner, session, the versioned routing-policy
digest and the actual local embedding descriptor. Policy identity includes
configured inputs, effective Router configuration, default provider and model
routes, as well as the history descriptor when attached. The current learning
policy distinguishes actual serving attribution and terminal status from the
older policy; old records are not silently relabeled into the new domain.
PostgreSQL history tables are also isolated by the actual message-event table.

Reads require both historical source and terminal sequences to precede the
current input. A domain is bounded to 1,024 history records with at most 2,048
vector dimensions; excess capacity or corrupt/oversized rows are explicit errors,
not partial history. Failed observations count toward the capacity bound even
when they do not contribute successful-neighbor votes. Each terminal has a
permanent idempotency receipt; replay does not create another observation and a
conflicting receipt is refused.

Learning derives the actual serving provider/model from the authenticated
terminal's physical attempt trace. A successful provider attempt does not alone
make the overall turn successful: terminal failure remains a failed observation.
All-failed traces and outcomes without a physical attempt are refused for serving
training. The current training consumer does not collect statistics for every
failure path. Each model retains a window of
at most 100 outcome observations for local success metrics, while the durable
receipt outlives that window. Metrics are published only after the database
transaction confirms the update. Owned cleanup jobs retain real backend work
through cancellation using one absolute deadline; an uncertain write remains
failed/unconfirmed rather than being treated as rolled back or free.

Execution observations retain at most 64 tool-call samples and 128 provider-attempt
samples. Counters still describe every observed attempt. Overflow marks the trace
incomplete; its bounded terminal outcome remains partial with no coverage claim.
That partial outcome can be persisted and replayed without authorizing intentional
silence or inheritance of incomplete predecessor observations. The producer and
store share the 128-sample bound; execution budgets and cleanup acknowledgement
requirements are unchanged.

Terminal replay authenticates the historical input, both actual decision rows,
model transition and serving trace. Failed phases retain their fixed stage and
absence of a successful PMM admission. It never reconstructs a live selected-read
or provider grant. Ordinary records without the new optional Router projection
keep their previous serialized/MAC form. Readers that do not recognize the new
projection reject it; this is one-way compatibility, not a guarantee that an old
binary can replay newly written records.

The embedded API creates a new session for each new Agent binding. Repeated
turns on the same Agent share its scoped statistics. Reconstructing an Agent or
restarting its process does not resume an earlier session's statistics, even
with the same database. There is no public cross-process session restore API.

The 2026-10-04 refusal-only AutoMix state is superseded by the 2026-10-05 source
addendum. Supported premium calls reuse the actual registered provider instance,
validate its pinned model binding and obtain separate accounting within the same
original deadline and allowance. Known main usage is settled first with the same
terminal settlement ID; main trace and charge are not rewritten as premium usage.
Failure or unknown premium completion retains the cheap result and the actual
auxiliary liability. A successful premium reply has a verified served-outcome
link, with an authenticated event for recorded replay. Recording-off may retain
only a private current-owner observation, without claiming durable replay.

Supported configured embeddings and Channel vision summaries likewise require
purpose-specific owned accounting and actual transport matching. An ordinary
chat decision alone is insufficient. The non-image Channel path needs no
auxiliary summary. Main-chat aggregate settlement is not represented as complete
per-physical-attempt accounting. Unknown Router learning models do not acquire
invented priors; existing configured-policy boundaries remain in force.

These are local candidate source contracts, not deployment or acceptance claims.
Final integration, database behavior, cancellation/crash recovery, feature and
entrypoint parity, performance and independent source/service reviews remain
pending. Installing a binary, restarting a service or changing real configuration
requires the existing separate operator authorization.
