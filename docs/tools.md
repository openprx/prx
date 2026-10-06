# Tools

Built-in and dynamically discovered tools are organized by category. The exact
provider-visible count is selected by the source-bound turn decision, configuration,
runtime health, MCP metadata, and the active WASM generation. The resulting immutable
`ToolSpec` snapshot controls provider representation and execution for that
iteration; see [Unified turn context](unified-turn-context.md).

The complete static public inventory contains 55 tool names. Runtime discovery
can add Skill, MCP, and WASM aliases. MCP aliases remain in the discovery
catalog and are resolved by exact name; the default MCP tool surface contains
only `mcp_call`. Its schema does not enumerate configured servers or tools.

| Category | Tools |
|----------|-------|
| **Capability discovery** | `capability_search` (source-authorized Execute turns only) |
| **Shell & Files** | `shell`, `file_read`, `file_write`, `file_edit`, `git_operations` |
| **Web** | `web_search_tool`, `web_fetch`, `http_request` |
| **Memory & Documents** | `memory_store`, `memory_recall`, `memory_search`, `memory_get`, `transcript_history_lookup`, `memory_forget`, `memory_reindex`, `chat_profile_update`, `document_search`, `document_get_chunk`, `document_ingest`, `document_sync` |
| **Skills** | `skills_list`, `skill_read`, `skills_manage`, `skill_execute`, plus up to 32 declared skill-tool aliases |
| **Messaging** | `message_send`, `stay_silent` (smart group-reply turns only) |
| **Sessions** | `sessions_spawn`, `sessions_send`, `sessions_list`, `sessions_history`, `session_status`, `subagents`, `delegate`, `managed_session` (`prx chat` only) |
| **Scheduling** | `cron` (calendar/time scheduling), `xin` (autonomous task and durable Goal/Step workflows), `chat_schedule` (`prx chat` only) |
| **Images** | `image`, `image_info` |
| **MCP** | `mcp_call`, `mcp_status`; discovered aliases require exact turn selection |
| **WASM Plugins** | `wasm_plugin_call`, `wasm_plugins_status`, `wasm_plugins_manage`, `wasm_plugin_reload`, plus up to 32 aliases when compiled with `wasm-plugins` |
| **Hooks** | `hooks_status`, `hooks_manage` |
| **Remote Nodes** | `nodes` (control paired devices — camera, screen, location, run commands) |
| **Infrastructure** | `gateway`, `config_reload`, `proxy_config`, `agents_list` |
| **Integrations** | `composio` (1000+ OAuth apps), `pushover` (notifications) |

Fresh file-write approval in the candidate runtime uses a dedicated argument
collection path in recorded Chat and Console sessions. Under supervised
policy, explicitly name `file_write` or `file_edit` in the request. When that
operation is otherwise admissible, one main-model response supplies the exact
proposal for durable owner review. The waiting reply contains its decision
identifier. Inspect and review it, then explicitly resume the stored call;
file contents change only after the fresh resume passes its execution checks.
Plan mode and unsupported review adapters reject collection. Filesystem
mutations remain outside capability discovery. See [Runtime security](security.md#approval-is-exact-durable-and-single-use)
and [Console decision approval](web-console.md#decision-approval-and-resume).

Console file paths refer to the authenticated owner's private space in the
candidate runtime. Read, write and edit share that namespace; ordinary Chat
continues to use the shared workspace. Private storage availability does not
waive approval, ReadOnly or Plan restrictions. See [Console private files](web-console.md#private-files)
for storage, path and platform restrictions.

`capability_search` accepts a nonempty `query` of at most 1024 UTF-8 bytes
and an optional `limit` from 1 to 5 (default 5). Each hit carries its canonical
tool schema, capability ID, schema fingerprint and catalog generation. The
shared turn engine exposes this control tool only for an Execute decision
with a legal discoverable family, subject to source exclusions and policy.
Discovery eligibility is retained independently of the prediction candidate
limit and the original input's length. An oversized original input cannot be
used directly for an initial search hop; a later model-supplied search query
must independently meet the 1024-byte limit.
Answer and delegation directives do not acquire discovery merely because the
tool is registered. Search uses the indexed, source-filtered catalog and does
not refresh external backends. Returned references are retained only in that
live turn and cleared when it closes. A search result alone grants no execution
authority; the tool requires an execution-service-issued invocation port and
rejects direct calls without it.
When the model subsequently calls an offered public name or exact capability
ID, the engine rechecks that specific reference before dispatch. Passed
definitions are appended after the stable core for later iterations of the
same turn; the current provider request is not rewritten. A pending-approval
result is kept separate and forces the actual call through durable approval
without running the backend. Unknown, blocked and expired references do not
expand the executable surface. Closing the turn removes discovery offers;
retained admitted references serve only as authenticated terminal evidence.

The model-managed capability surface is built from the same core registry in
every model-running entrypoint:

| Entrypoint | Skills | MCP | Hooks | WASM |
|---|---:|---:|---:|---:|
| `prx chat` (TUI or plain) | yes | yes | yes | yes |
| `prx agent` (interactive or one-shot) | yes | yes | yes | yes |
| standalone IM channel supervisor | use only¹ | call only¹ | run only¹ | call only¹ |
| gateway/webhook turns | yes | yes | yes | yes |
| delegated/session-worker turns | yes | yes | yes | yes |

¹ The registry a channel supervisor builds is the same one, and every capability
still *runs*: skills are usable, MCP and WASM plugin tools are callable, and
hooks fire on the turn exactly as they do in terminal chat. What
`channel_exclude` removes is the operations surface — the tools that manage or
inspect those subsystems — because a channel turn is driven by a conversation
partner rather than by the operator. The default excluded set is
`DEFAULT_CHANNEL_EXCLUDED_TOOLS` (`src/config/schema.rs`):

`wasm_plugins_manage`, `wasm_plugin_reload`, `wasm_plugins_status`,
`hooks_manage`, `hooks_status`, `proxy_config`, `config_reload`, `gateway`,
`mcp_status`, `nodes`, `git_operations`, `skills_manage`, `memory_reindex`,
`document_sync`, `document_ingest`.

Nothing else is hidden by the channel surface. See
[Per-turn capability routing](#per-turn-capability-routing) for how to override
it.

## Per-turn capability routing

The current local candidate uses the shared turn decision to select exact
capability references from a pinned runtime catalog. The original recorded
input belongs to the source proof; recalled memory and later provider history
do not replace it. Selected references carry a catalog generation and schema
fingerprint, and execution rechecks their source, policy and current definition.
The candidate remains under verification; see [Decision plane](decision-plane.md).

`always_exclude` removes names from the eligible surface. `always_include`
prefers direct publication and can restore a default channel exclusion, but it
does not bypass explicit exclusions, model allowlists, readiness, child scope,
cost admission or approval. An exact-model `model_allowlists` entry intersects
the eligible set; an empty list disables tools for that model. Matching follows
the tool backend's supported names: accepting one alias admits that root's
published aliases, while explicit exclusions still apply. Each admission and
execution provenance check reevaluates the current backend support.

A request in another language does not expose the full registry. The decision
may select discovery, or an admitted Execute turn may expose `capability_search`
as described above. Search returns bounded offers from eligible families. An
offered capability must pass its own admission before it can execute or join
the appended surface. The old keyword selector and cumulative session union
are absent from the production path. Tools offered on an earlier turn do not
become authority for a later turn.

Within one turn, the selected core is stable and newly admitted discoveries
are appended after it. Both segments use canonical ordering and exact schema
fingerprints. A later turn obtains its own decision and may have a different
core; this contract does not promise a byte-identical tool prefix across every
turn in a conversation. Clearing history is not a substitute for revoking an
approval or closing a live execution scope.

If a tool is missing, check its runtime registration and readiness, explicit
and channel exclusions, model allowlist, inherited child boundary, then the
current decision and any pending approval. Naming a tool in text is not an
approval, and `always_include` does not guarantee that a decision selects it.
For example, the following makes these tools candidates for direct publication
without granting permission to execute them:

```toml
[tool_tiering]
always_include = ["memory_recall", "document_search"]
```

Skill publication supplies metadata from the workspace catalog. Instruction
bodies are loaded through an admitted `skill_read`; they are not injected merely
because a skill was named previously. Memory enrichment likewise comes from
selected Retrieve results rather than unconditional recall. Memory, Document
and Session evidence retain distinct types and source-owned access scopes;
see the [backend support matrix](decision-plane.md#candidate-memory-backend-support).

IM channel sessions (Signal, Telegram, Discord, WhatsApp/wacli, Slack,
Mattermost, iMessage, Matrix …) additionally run under `channel_exclude`, which
hides the operations/development surface from a conversation partner:

`wasm_plugins_manage`, `wasm_plugin_reload`, `wasm_plugins_status`,
`hooks_manage`, `hooks_status`, `proxy_config`, `config_reload`, `gateway`,
`mcp_status`, `nodes`, `git_operations`, `skills_manage`, `memory_reindex`,
`document_sync`, `document_ingest`.

These names are folded into that turn's `always_exclude`, so they stay hidden
even when the message names their capability. Terminal chat, the
gateway/webhook surface and the web console are unaffected — those callers are
the operator. To put one back on channels, name it in `always_include`; to turn
the whole default off, set `channel_exclude = []`:

```toml
[tool_tiering]
# Re-enable repository access for channel conversations.
always_include = ["git_operations"]
# Or drop the channel default entirely.
# channel_exclude = []
```

Delegated and session-worker turns still honor an agent's explicit
`allowed_tools` boundary. `allowed_tools = ["*"]` inherits the complete parent
eligible capability set, including dynamically discovered MCP and WASM aliases;
a narrower allowlist intentionally exposes only the named capabilities. Task
children proxy the live parent registry. Process workers reconstruct native,
Skill, MCP, WASM, Hook, memory, network, scheduling, session orchestration,
daemon messaging, and Gateway tools from the parent's sealed configuration.
Each child still obtains its own decision within that inherited boundary.
UI-owner controls such as attaching a TUI viewport remain in the UI process and
are not agent execution capabilities.

In the current candidate, process children carrying causal authority pass
worker admission. Their transport binds
the child capability subset, durable input identifier, and parent decision digest
inside the worker signature. Parent constraints carry a separate decision-key
signature. The worker loads existing authority without creating keys or repairing
permissions, verifies the strict subset and inner seals, and reads the original
owner-scoped parent decision to compare its actual record digest. A separate
keyed binding covers the parent's effective configuration, logical workspace,
configuration path and generation metadata without transporting configuration
values. A worker may restore that generation only when its loaded source and
effective values match; changed disk or environment-derived values are refused. The full parent capability catalog also travels as an opaque signed snapshot;
restoration preserves its generation, policy, schemas and bindings while rebuilding
lookup indexes. Restored metadata does not establish parity with the worker's
actual executors and does not transfer satisfied approvals. The parent-recorded
spawn input has a separate decision-bound MAC. The child path reads that exact
owner-scoped durable event and validates its content and lineage instead of
creating a second initial input event. With private worker memory, source
verification opens the parent's configured backend under the verified logical
workspace; tools keep their private execution memory. Shared-memory workers reuse
their existing fabric after checking its workspace. Full source-proof admission
and process result reconciliation remain part of the pending worker integration.
Conditional shared worker segments settle provider usage individually. Their logical
worker terminal omits provider billing and must be read back from the logical
source store before publishing a result. Cross-process usage projection and
interrupted-worker recovery are still pending; this does not enable admission.
Worker completion transport includes a separately signed inventory of the logical
terminal and confirmed segment event references and digests, including segments
without metered usage. The signature binds the original spawn input, parent
decision and returned result. The parent must still verify the inventory and
read the durable events before treating any usage as confirmed.

For granted process workers, the reviewed local scope repair binds logical-task
completion to the original authenticated spawn input, preserving its source,
owner, visibility and complete lineage. Worker model segments and worker-result
events retain their separate worker scope. Logical terminal issuance and the
final draft use the same original task identity; receipt, backend, signature and
scope checks remain mandatory. This does not create a new grant or accept an
unsigned completion. For a native issued child grant, a new child session is
derived from the authenticated parent session and the runtime-reserved child run
ID before recording the input. The original parent session remains separate
lineage. Ordinary and operator spawning retain their existing session behavior;
recovery uses the recorded child session without rewriting existing records.
Runtime validation remains pending. No configuration or
stored-data migration is required; reverting the scope repair restores the prior
refusal when the worker projection differs from the original task identity.

The signed inventory includes the number of recorded segment inputs. A successful
result must cover every index exactly once; duplicate event references and
noncanonical or out-of-range segment identities are refused. Failed results may
retain an explicitly incomplete inventory. The parent verifies the completion
MAC against the original issued grant and spawn input before processing it.
The parent then reads each logical and segment terminal from owner-scoped logical
source storage, compares signed event digests and lineage, and verifies segment
terminal replay against the issued parent decision and digest. Logical success
also requires the durable history projection to match the redacted returned
output, with no duplicate logical provider billing. Failed results must match the
durable error summary and have no success history. Only the verified completion
can project segment usage into the parent registry, preserving settlement ids,
provider/model attribution and recorded costs on both success and failure. The
last segment token summary does not determine granted worker billing.
Process registry merges validate all identified settlements before mutating state:
identical repeats are deduplicated and conflicts are refused. Repeated terminal
confirmation must preserve the prior status and finalization, and unidentified
legacy usage cannot be replayed. Result events and announcements follow a
successful registry commit. After confirmed termination or exit without a result, the parent recovers known
segment settlements through owner-isolated indexed idempotency lookups and
authenticated terminal replay. Recovery follows this task's consecutive segment
identities and never scans the workspace event catalog. Unsupported event
backends fail explicitly. The result warns that provider usage may be incomplete,
even if all visible segments have terminal records. An external process observer owns the runtime work guard. Missing confirmation
and granted-worker verification errors produce an explicit unconfirmed control
state; result publication is withheld and the work record is retained. Only
natural completion or confirmed termination permits work-record retirement.
Session cleanup retains unconfirmed and failed-termination rows, and kill commands
report that confirmation is unavailable. The external observer also retains an opaque verified completion in memory.
If the monitor stops before registry confirmation, it retries that exact run
result and settlement set through the conflict-checked commit, then acknowledges
cleanup only on success. Persisted restart recovery and repair of an already
unconfirmed control state remain pending.
The process ledger marks the worker leader reaped only after confirmed exit or
successful cleanup. Failed termination and cleanup errors retain the process
record and worker workspace; returning from the lifecycle future alone cannot
authorize their retirement. This does not infer descendant cleanup from a
reaped leader or reuse a relinquished process-group identifier.
For granted workers, the outer runtime cleanup must drain successfully before
writing `worker.completion.confirmed` to the logical event store. This idempotent
record contains redacted output/error text and a separately recomputed signature
over the completion inventory. The worker verifies owner-scoped durable readback
before returning its stdout result. After confirmed process termination or exit without stdout, the parent reads
this journal by owner and run, validates its scope and a dedicated journal MAC
domain, and reuses the logical/segment replay verifier. A valid committed result
takes precedence over a late termination or lost stdout; absent journals fall
back to partial usage recovery. Ordinary stdout signatures cannot authorize a
cleanup journal. Daemon restart discovery and unconfirmed-state repair remain
part of the unfinished migration.
The external observer retains the logical recovery context and an in-memory
exit acknowledgement set only by the process owner after confirmed reap. If
result parsing or verification is interrupted, it can read and verify the durable
completion after joining the monitor. Journal presence or a PID probe alone does
not set that acknowledgement; missing exit evidence leaves recovery unconfirmed.
The observer may repair an unconfirmed or termination-failed control state only
with an opaque proof joining its owned exit acknowledgement and verified result.
The repair must match registry owner, run, exact status and usage proposal.
Conflicting settlements and unidentified prior usage block repair before mutation;
already-confirmed natural completion or termination remains immutable.
Before sending a granted worker its manifest, the parent writes and reads back
a signed `worker.recovery.registered` record. It binds owner/workspace, run and
session, original input event and keyed digest, parent decision digest, config
generation and the owned worker PID. It contains no provider credential or raw
task text. Registration failure follows owned-child cleanup before any task
input is sent. The PID is provenance for future read-only reconciliation, not
a reusable signaling capability; restart discovery and liveness checks remain
pending.
The launch reader loads existing key material without creating or repairing it,
validates the signed event scope and original input MAC, and re-reads the
owner-scoped parent decision's actual digest and Delegate lineage. Startup uses
this same reader before dispatch. Its verified return value is read-only
provenance and cannot become a model or tool execution source proof.
Completion verification consumes a narrow borrowed replay context containing
only the original event, owner/workspace, parent digest and verification key.
The live path derives it from the issued grant; durable recovery derives it from
the verified launch record and uses the same terminal checks. Recovery therefore
does not reconstruct provider selection, tool catalogs or execution grants.
Parent provenance carries separately signed source identity, routing input,
policy and the exact inherited memory scope. Restoration checks the original
owner-scoped parent decision and event plus child causation links. This provenance
cannot serve as a model execution proof; provider authority must be established
separately for the actual child provider. The exact PMM catalog is also signed
and restored, including curated declarations, discovery support states and times,
dialects and route references. Worker grant assembly combines this evidence with
the verified parent provenance, configuration, capability scope and durable input;
it does not refresh discovery evidence from a newer cache. Actual provider and
executor binding is still required before the worker can execute.
The causal worker selection carries its original provider, model or hint, and
credential reference. The parent resolves the sealed connection from that exact
selection; the worker verifies target, credential and root-connection provenance
against the restored PMM view before constructing its provider once. Its child
source proof is tied to that actual provider instance. Ordinary workers retain
their existing provider validation order. The conditional worker service is
published from its actual filtered registry and logical decision storage under
the outer cleanup owner. Publication checks child capability schemas, effects
and dynamic bindings against actual executors; prompt tool eligibility uses the
shared PMM gate. Incoming grant and provider binding remain in the worker context
for nested operations. Segment execution and terminal wiring are still pending. Missing or inconsistent source/authority pairs are
rejected; a transport signature alone does not authorize execution. Ordinary
workers without causal authority retain their existing manifest format.

When an explicit child allowlist names a dynamic alias that has not been
discovered yet, PRX refreshes the dynamic registries before resolving the
allowlist. Multiple aliases exported by the same MCP, WASM, or Skill router are
represented by independent public-name proxies and remain executable.

Every provider-visible tool uses an object JSON Schema. Multi-action tools
publish discriminator-specific required fields through canonical conditional
clauses. `ToolExecutionService::execute` — the single path every native, skill,
MCP alias and WASM plugin call takes — evaluates that same published schema
against the model's arguments before the policy decision is acted on, before any
approval prompt, and before the executor runs. Validation covers nested required
fields, scalar types, enums, constants, string/array lengths, numeric bounds,
union types, unknown-field rejection where the schema closes its property set,
alternative required-field sets, `patternProperties`, `if`/`then`/`else` at any
schema level, and local `$ref` entries resolved against the document's own
`$defs` / `definitions` (with cycle detection; a reference that points outside
the document constrains nothing, and the subtree *below* a cycle is
unconstrained as well — only the level at which the cycle closes is enforced).
A structurally invalid call is rejected
with an `InvalidArguments` outcome that names every offending path, is recorded
in the tool execution audit like any other terminal outcome, and never reaches
the backend. Executor checks remain as defense in depth.

Validation is fail-open on JSON Schema keywords PRX does not model and
fail-closed on the ones it does: a dynamic MCP or WASM contract that uses
`pattern`, `format`, `propertyNames` or similar keeps working, and only the
modelled constraints bind. `type: "integer"` follows the specification's
numeric model, so a whole number a provider spelled as `1.0` is an integer.
`enum` and `const` use the same numeric model, so one document cannot accept
`1.0` for `type: "integer"` and refuse it for `enum: [1]`.

Because `integer` accepts a float spelling but executors read integers with
`as_u64()` / `as_i64()`, the service rewrites integral floats into JSON integers
after validation and before dispatch, against the published schema: nested
objects, array items, `patternProperties`, an `additionalProperties` schema and
the taken side of an `if` / `then` / `else` are all covered. A non-integral
value is a violation and is rejected, never rewritten.

Compilation is bounded. `$ref` resolution unwinds its stack, so a non-cyclic
document that references the same definition from many places can expand
exponentially, and MCP or WASM contracts are caller-supplied. Past the node and
depth budgets the remaining subtrees compile to no constraints and a warning is
logged; the bound counts work, not time, so a given document always costs the
same.

`oneOf` has one rule worth stating, because fail-open changes what a count
means. Where every branch is expressible in the modelled subset — which covers
every contract PRX itself publishes — "exactly one branch matches" is enforced
as written. Where a branch turns on a keyword the validator does not model, two
branches can compile to the same weaker rules and match the same document, so
the multiplicity test relaxes to "at least one": an undecidable `oneOf` still
rejects a document no branch accepts, but never rejects one for matching too
many. A key matched by `patternProperties` counts as declared, so
`additionalProperties: false` does not reject it.

Each contract is compiled once per provider dialect, public tool name and
complete schema document, identified by a SHA-256 of the three. Two documents
advertised under one name each keep a cache entry, so an MCP router that swaps
servers or a WASM alias reload hits the cache both ways instead of recompiling
per call, and a full cache evicts its least recently used entries rather than
clearing itself.

The document enforced is the one the caller was actually shown. Tool schemas do
not always reach a provider verbatim: Gemini's API rejects `additionalProperties`,
`minLength`, `maxLength`, `pattern`, `format`, `minimum`, `maximum`, `minItems`
and several more, and Anthropic does not resolve `$ref`, so PRX rewrites the
schema for those providers before the request goes out. Validation replays the
same rewrite, so a constraint a provider strips is never enforced against a
model that was never shown it — without this, every tool that closes its
property set rejected otherwise-correct Gemini calls. Everything that survives
the rewrite (`type`, `enum`, `required`, conditional clauses) stays enforced on
every provider, and OpenAI and OpenAI-compatible endpoints, which are sent the
published document unchanged, are validated against it unchanged.

The gateway MCP server (`POST /mcp/v1/tools/call`) runs through the same
service. An external MCP client is shown the published document, so it is held
to the published document, and its request goes through the same policy
decision, approval gate, schema check and audit record as a model-issued call.
Approval is consulted only when the autonomy level asks for it
(`Supervised`), and is then fail-closed: a remote client cannot answer a prompt,
so the call is refused rather than auto-approved. At the default `Full`
autonomy it is not consulted at all, so it is not the boundary for external
callers. That boundary is the owner-isolation-aware list below.

**Only owner-isolation-aware tools run for external MCP callers.** Every
external MCP caller runs owner-isolated (see "Owner-isolated reads for external
MCP callers"), and the execution service refuses an owner-isolated caller any
tool that does not enforce per-owner isolation, before policy and approval are
evaluated. The list is explicit (`OWNER_ISOLATION_AWARE_TOOLS` in
`src/tools/execution.rs`): `memory_search`, `memory_recall`, `memory_get`,
`document_search`, `document_get_chunk`, `sessions_list`, `sessions_history`,
`memory_forget`, `memory_store` and `document_ingest`. The name must also be
served by the tool of that name, so a dynamic MCP, WASM or skill alias that
publishes a listed name from another backend is refused too. Any other name in
`mcp_server.exposed_tools` — `document_sync`, `shell`, `cron`, `message_send`, `sessions_spawn`, any dynamic
alias — is not advertised by `GET /mcp/v1/tools/list`, `initialize`,
`/mcp/v1/list_servers` or the A2A capability lists, is refused by
`POST /mcp/v1/tools/call` with 403 `tool '{name}' is not available to an
owner-isolated caller: it does not enforce per-owner isolation` before any
idempotency reservation or identity binding is written, and is named by a
warning when the configuration loads. The entry does not fail config load, so
a daemon whose configuration exposed such a tool still starts. A tool joins the
list only after it scopes its own effect by owner. `session_status` and
`sessions_send` scope their effect by owner for in-process turns, but the
gateway does not register them, so they are kept off the list: exposing them
over MCP answers the 403 above, before anything is written, rather than a 404
after the caller's identity binding.

PRX publishes its own signed A2A Agent Card at `GET /.well-known/agent.json`
(the verification keys are at `/a2a/v1/.well-known/jwks.json`). It does not
fetch or verify the Agent Card of any other agent: nothing in PRX consumes a
peer's card. A peer is trusted by its own identity, checked against
`a2a.allowed_peer_issuers` and the SPIFFE trust domains in
`a2a.trusted_trust_domains`, never by a card it publishes.

That client may send an `Idempotency-Key` header with a tool call. The key is
scoped to the authenticated caller (`{owner id}:{key}`) and reserved in the same
durable execution ledger the Chat and Agent entry points use, so a retried POST
of the identical request runs the side effect once and replays the first
outcome, while reusing the key for a different command is refused. Two callers
that pick the same key string both execute. Without the header nothing is
deduplicated: a retry executes the tool again. When no ledger can be opened, a
keyed call to a tool with side effects is refused rather than run twice — the
response says the ledger is unavailable — and a read-only tool still runs.
`mcp_server.rate_limit_rps` is a per-caller budget, in requests per second,
enforced on `POST /mcp/v1/tools/call` and keyed by the same owner id; `0`
disables it. `GET /mcp/v1/tools/list` reports each admissible exposed tool's
real effect (`readonly` or `act`) rather than assuming the list is read-only,
and a side-effecting entry in `mcp_server.exposed_tools` is warned about at
config load.

An exposed name also has to be *resolvable*. Discovery projects the admissible
part of `mcp_server.exposed_tools` — the entries on the owner-isolation-aware
list — from configuration, while
`POST /mcp/v1/tools/call` admits a request only when the gateway's own tool
registry answers to that name; a name that is advertised but never registered
is refused with 404 `exposed tool is not available`. That registry is the one
of the current configuration generation, pinned once per call exactly as a
webhook turn pins it, so a hot reload that adds, removes or reconfigures a tool
takes effect on the next `POST /mcp/v1/tools/call`; before this the call kept
resolving against the registry built at gateway start. Both gateway launch paths
— the daemon (rebuilt on every hot reload) and `prx gateway` — register their
tools from one shared body, so the default-exposed set resolves on either.
`sessions_list` and `sessions_history` on the gateway read the sub-agent run
registry that this gateway's own `sessions_spawn` writes to, together with the
owner-qualified memory projection described below; they are not a separate,
empty registry that would answer every caller with an empty list. One
conditional exception remains: `memory_recall` is withheld from the registry
whenever `memory.acl_enabled` is true, so with memory ACL on that name is still
advertised and answers 404.

### Owner-isolated reads for external MCP callers

The execution envelope an authenticated MCP caller runs under is keyed on the
external *subject*: the session key is `mcp:{subject}` and the sender and
recipient are the subject itself. The issuer appears in exactly one place, the
owner id `owner:{workspace}:agent:{issuer}:{subject}`. Two identities that
present the same subject under different issuers therefore share a session key
and a sender, and a read that admits `session`-visibility rows by session key
and `private`-visibility rows by sender would hand each of them the other's
history through the default-exposed `sessions_list` and `sessions_history`
tools.

An externally authenticated envelope is therefore marked owner-isolated, and
that mark travels to every tool through the `_zc_scope` block the execution
service re-authors from the envelope. Under it, the `session` and `private`
arms of every principal-scoped read in both the SQLite and Postgres adapters
additionally require `owner_id` equality — the legacy-key read merge included —
and a principal that is owner-isolated but carries no owner id admits no
`session` or `private` row at all rather than falling back to sender matching.
The `workspace` arm of every event read — message events since / recent / in a
time range, the shared- and session-context feeds, memory events since / recent
— is owner-qualified the same way for an owner-isolated caller, so such a
caller never reads a `workspace`-visibility event it does not own; `global` is
the only event tier that stays shared, as `public` is for memories and
documents. For every other reader the statement and its bind list are
byte-identical to before. The session read tools additionally apply the owner
boundary to what they are handed — `sessions_spawn` records a run's
`task.spawned` lifecycle event, its request message (which carries the full
task string) and its sub-agent result message at `workspace` visibility — with
the same rule in both places: an owner-isolated caller sees only runs it owns, in the in-process run
registry and in the recovered memory projection alike, and a caller that carries
no owner id, or a run whose rows carry no owner, is admitted to nothing.
`sessions_history` for a run the caller does not own answers `No run found`,
exactly as it does for a run that never existed.

### Sub-agent runs are private to the owner that started them

The same run boundary applies to every caller, not only to external MCP
callers. A run is tagged with the owner of the turn that spawned it: the
authenticated owner id when the turn carries one, otherwise the
`owner:{workspace}:{channel}:{sender}` identity composed from the turn's own
channel and sender — the same id the spawn has always recorded in its lifecycle
events. `sessions_list`, `sessions_history`, `session_status` and
`sessions_send` then show or act on a run only when it belongs to the caller's
owner, computed the same way, in the in-process registry and in the recovered
memory projection alike. That covers gateway webhook turns, Web Console chat
turns, IM channel turns and sub-agent workers: each sees the runs its own
owner started, and a run started by another webhook credential, channel user or
MCP caller answers `No run found`, exactly like a run that never existed.

A webhook turn's owner is bound to the credential the request authenticated
with — the paired bearer token when `gateway.require_pairing` is on, otherwise
the standalone `X-Webhook-Token`, otherwise the HMAC signing secret, otherwise
the single `public` principal — with the body's `reply_target` only a
sub-namespace under it: `owner:{workspace}:webhook:{credential}:{reply_target}`,
where `{credential}` is the credential kind (`bearer`, `token` or
`signing-secret`) and a 128-bit domain-separated digest of the secret
(SHA-256 over `prx-webhook-owner-v1`, the kind and the secret), never the plain
hash that `gateway.paired_tokens` stores as the token's verifier. Naming another
client's `reply_target` therefore reaches nothing unless the caller also holds
that client's credential. Everyone holding one credential is one trust domain:
`reply_target` separates their runs only as far as they choose to assert it,
and with pairing off and no webhook token or signing secret every caller shares
`public`. Webhook runs recorded before this change carry the old
`owner:{workspace}:webhook:{reply_target}` owner and are visible to the
operator surfaces only. The webhook conversation is keyed the same way,
`gateway:webhook:{credential}:{reply_target}`, so a caller with another
credential that sends the same `reply_target` gets a separate conversation with
no shared history. Conversations recorded before this change under
`gateway:webhook:{reply_target}` are not migrated or merged, because they
cannot be attributed to a credential: each webhook client's continuity restarts
once after the upgrade. With no pairing, webhook token or signing secret
configured, every webhook caller shares the `public` owner and one session
space per `reply_target`, by design. Rotating a credential — re-pairing,
revoking or re-issuing a token, or changing `X-Webhook-Token` or the signing
secret — changes the owner and the conversation key with it: that client's
earlier runs become visible to the operator surfaces only and its conversation
restarts, exactly as after the upgrade. Every paired bearer token is a full
operator credential on `/api` (configuration and secrets, skills, hooks,
sessions, jobs and their results, tasks). The per-credential separation above
applies to `/webhook` traffic only and is not a boundary between holders of
paired tokens. The
management actions follow the same rule: `sessions_spawn` `list`, `history`,
`kill`, `steer` and `join`, and `subagents` `list`, `kill` and `steer`, reach
only the caller's own runs (and `join` only a batch its own owner launched),
and a refused run gets the same answer as a missing one, so another owner's
run id cannot even be confirmed. `sessions_spawn` `chat_assign` is not a run
lookup: it hands work to a live `prx chat` session and is authorized by its
own default-deny `autonomy.scopes.assign_owners` / `assign_allow` rules.

The `[Recent shared workspace events]` block that a process-mode sub-agent
worker receives in its prompt follows the same rule. It used to carry the 24
most recent `workspace` events of the whole workspace — other owners' spawn
task text and sub-agent results included, with no prompt injection needed to
get them there. It now carries only events of the reader's own owner from its
other sessions: the shared-context feed requires the owner on its `workspace`
arm for every reader, and a worker reads for the owner that spawned it (the
owner in its manifest). Events without an owner, and a reader without an owner
id, contribute nothing to it.

The local-process operator keeps the workspace-wide view: turns driven from the
terminal UI and from the `prx agent` CLI (single-shot or REPL) see and manage
every run. The runtime recognises both from the scope it authors itself — the
terminal UI's `terminal`/`user`/`terminal:user` tuple and the CLI's
`cli`/`agent`/`agent:local` tuple — so no network surface can claim either.
Cron agent jobs and xin tasks run through the same CLI entry point, but carry
the CLI tuple — and so keep the operator's view — only when the local operator
created them; every other job runs as its creator (see
[Scheduled job origin](#scheduled-job-origin)). Runs recorded before this
change without an owner are visible to the operator only. A call that bypasses
the execution service entirely (an in-process call with no trusted scope) keeps
the unrestricted view it always had.

For external MCP callers the owner boundary also covers the `memories` table, which the default-exposed
`memory_search`, `memory_get` and `memory_recall` tools read. For an
owner-isolated caller every **non-public** arm of the memory visibility scope
additionally requires `owner_id` equality, on the `sqlite` and `postgres`
backends; `visibility = 'public'` stays shared and `sensitivity = 'secret'`
stays excluded, exactly as before. A caller that is owner-isolated but carries
no owner id reaches no owned row at all — on both backends alike, as an empty
result rather than an error — and a stored row with no `owner_id` is
unattributable and therefore reaches no owner-isolated caller either. Nothing is
migrated or rewritten to make that true, so a rollback restores the previous
visibility immediately.

An embedded agent's legacy `MemoryLoader` callback runs only after a Memory
retrieval is selected and validated. It receives a bounded read view tied to
the authenticated source workspace and owner. Non-public rows must match that
owner; public rows remain shared under the existing visibility rules.
This read scope is separate from the semantic write context and does not change
that context. A conflicting workspace or owner is rejected before the callback;
ownerless historical rows are not attributed to the caller. Read counts, result
bounds and the original decision deadline still apply. Reverting this read-scope
repair restores the earlier failure for embedded write contexts without an owner.

The shared SQL predicate is the owner contract on both backends. On PostgreSQL
the row-level-security policy over `memories`, `document_chunks` and
`embedding_cache` is a floor under it. For an owner-isolated caller the policy
now matches the predicate: each such transaction sets `prx.owner_isolated`,
and the policy then admits only the caller's own rows, closing the shared
`workspace`/`global` read arm. For every other caller the policy keeps its
tenant-wide read of `workspace` and `global` rows, intersected with the
predicate as before. Inserted and updated rows must carry the caller's owner
either way. Owner-isolated callers can update or delete only their own rows;
for every other caller the policy also admits other owners' `workspace` and
`global` rows as update and delete targets (a delete has no post-image to
check), so for those callers the SQL predicate, not the policy, decides which
of them a write reaches. A separate,
read-only (`FOR SELECT`) policy lets every caller in the same workspace read
another owner's `public` memory row, as the predicate and the SQLite backend
already do; it never allows updating or deleting that row. Document chunks
have no `public` tier. The PostgreSQL side of these predicates and of the policy text is pinned by
statement-text tests. Its behavioural tests need a PostgreSQL instance and have
not been run on the release host.

The delete is bounded by the same owner. `memory_forget` is **not** on the
default MCP surface, but it is registered like any other tool and is on the
owner-isolation-aware list, so an operator can expose it with
`mcp_server.exposed_tools`. It therefore takes the isolation bit and the
owner id from the same service-authored scope the read tools use — never from a
model argument and never from a config flag — and on `sqlite` and `postgres` an
owner-isolated caller removes only rows whose `owner_id` equals its
authenticated owner id, at any visibility. That is stricter than the read
boundary in exactly one place, deliberately: a `public` row stays readable by
every caller, but deleting one is still deleting a row belonging to someone
else, so it is refused. A row with no `owner_id` cannot be proved to be the
caller's and is not deletable by an owner-isolated caller either, and a caller
that is owner-isolated but carries no owner id deletes nothing at all — on both
backends alike, reported as the ordinary `No memory found with key` answer
rather than an error. A caller that is not owner-isolated runs the statement and
the bind list it always ran, and a call that carries no trusted scope at all
still reaches the unscoped delete it always did.

The write is bounded the same way. `memory_store` is not on the default MCP
surface either, but it is on the owner-isolation-aware list, so an operator
can expose it. Memory keys are global, and every scoped write is an upsert on
the key that used to move the row to the writer. For an owner-isolated caller
the write now lands only on a key that is new or already the caller's own: the
row is attributed to the caller's authenticated owner id alone, and the upsert
carries an owner guard, so a key that belongs to another owner — or to no owner
— is neither overwritten nor taken over. That write fails with
`Failed to store memory: key '<key>' is not available to this caller`. The
refusal names no owner, but it does tell the caller that the key is in use.
An owner-isolated caller is refused the `conversation` category — the one the
runtime keeps transcripts and autosaves in, and whose near-duplicate rows the
self-system's memory evolution moves to trash workspace-wide — so it cannot get
other owners' conversation memories trashed as redundant; `core`, `daily` and
custom categories work. A caller that is owner-isolated but carries no owner id writes nothing, and
the backends that store no owner (`markdown`, `none`, `wasm:*`) refuse
an owner-isolated write instead of overwriting. `lucid` accepts it into its
local SQLite store only, with the owner, and never copies it to the Lucid
service. On PostgreSQL an RLS rejection
of the existing row surfaces as the same refusal. Writers that are not
owner-isolated keep the previous last-writer-wins upsert, statement for
statement.

What an MCP caller stores stays readable by the operator's own turns. With the
default `memory.acl_enabled = false`, the operator's in-process recall reads the
memory store unrestricted, so text an MCP caller wrote under its own owner id
reaches the operator's recall context. That is the operator's own store, and
exposing `memory_store` is an explicit opt-in through
`mcp_server.exposed_tools`; treat what external callers store there as
untrusted input.

Only the local operator's writes are mirrored into the workspace Markdown files.
`MEMORY.md` is one of the workspace files inlined into the system prompt of every
surface — the terminal UI, `prx agent`, webhook turns, the Web Console, IM
channels, sub-agents — so text placed there reads as the operator's own
instructions on every user's next turn. With `memory.acl_enabled = false`,
`memory_store` therefore mirrors a write into `MEMORY.md` (`core`) or the daily
log (`memory/<date>.md`, other categories) only when it comes from the terminal
UI or the `prx agent` CLI, or from a cron agent job or xin task the local
operator created. A `memory_store` from a webhook, channel, gateway, Web
Console, MCP or sub-agent turn, or from a job one of those created, stays in the
owner-scoped memory table, where
its owner can recall it. Other internal writers (autosaves, router and
self-system records, plugin storage, imports) are no longer mirrored either.
With the `markdown` backend the store *is* those files, so `memory_store` is
refused to every origin but the local operator there. A WASM plugin's
`memory.store` host call is not refused, though: on the `markdown` backend a
plugin's `core` write is still appended to `MEMORY.md` (any other category to
the daily log), so grant the `memory` permission only to plugins you would let
write the operator's prompt material. The mirror escapes line
breaks, backslashes and `*` in the key and the content, so a stored value can
neither start a line of its own in `MEMORY.md` nor pass itself off as another
key's `- [..] **key**:` entry. Lines already in `MEMORY.md` from before this
change are not rewritten; review the file once after upgrading.

The workspace-root `MEMORY_SNAPSHOT.md` export (rewritten whenever a SQLite or
`lucid` memory backend starts) follows the same rule: it carries only `core`
rows the local operator wrote through the terminal UI or `prx agent`, and rows
the operator's own process wrote with no caller context. Rows of MCP callers
and of every network surface are not exported, nor are rows a WASM plugin
writes through `memory.store`, which carry the channel `plugin:<name>`. Each entry's content is escaped
line by line, so pasted text can never open another entry, and the entry keeps
its owner and origin on an `*Origin: {..}*` line. Cold-boot hydration from the
snapshot restores that owner instead of writing the row unowned, and ignores
forged entry headers inside content. A current snapshot says so on a format
marker line right after its header. A snapshot without that line, written by
an earlier version, still hydrates exactly as it was written: its `*Origin:`
lines are not honoured and nothing in it is unescaped, so its rows restore
without an owner, as before, whatever owner such a line names.

The owner-scoped predicate exists only on those two backends: they are the ones
that store an `owner_id` per row. Every other leaf `memory.backend` value —
`markdown`, `none`, a `wasm:*` plugin, and any unrecognised name, which falls
back to `markdown` — has no per-row owner to compare against, so it **returns
nothing at all** to an owner-isolated caller rather than answering from an
unscoped read. `memory_search`, `memory_get` and `memory_recall` are therefore
empty for external MCP callers on those backends, and a line is logged naming
the backend and the operation. The same rule covers the delete: on those
backends `memory_forget` removes nothing for an owner-isolated caller and
reports that nothing was removed. In-process turns on those backends are
unaffected: they are never owner-isolated. The default backend is `sqlite`.

`lucid` is the exception, because it is not a leaf: it keeps its rows in a local
SQLite store that does carry `owner_id`, and forwards the owner-scoped read to
it. An owner-isolated caller therefore reads its own rows on `lucid` exactly as
it does on `sqlite`. What it does not get is the Lucid service half of the
answer: that returns unattributed context with no per-row owner, so it cannot be
scoped and is not consulted under isolation. A caller that is not
owner-isolated keeps the full local-plus-service answer, unchanged. The delete
is forwarded the same way: `memory_forget` from an owner-isolated caller on
`lucid` deletes the caller's own row from the local store (where its isolated
writes live) and no other owner's; a caller that is not owner-isolated keeps
the historical delete.

The same boundary covers the document chunk table, which the default-exposed
`document_search` and `document_get_chunk` tools read. For an owner-isolated
caller a chunk is visible only when its `owner_id` equals the caller's
authenticated owner id. The owning document's visibility does not admit it on
its own: a `workspace` **or `global`** document belonging to another owner is
not returned, a chunk with no `owner_id` is not returned because it cannot be
proved to be the caller's, and a caller that is owner-isolated but carries no
owner id reaches no chunk at all — on the `sqlite` and `postgres` backends
alike, as an empty result rather than an error. The two tools answer
identically about the same `chunk_id`: `document_search` does not return a
snippet of a chunk that `document_get_chunk` would then refuse. This applies to
the keyword, vector and fallback chunk queries alike. It matters because a
document is stored at `workspace` visibility unless the writer says otherwise,
and the context evictor ingests the evicted conversation transcript as a
document, so the chunk table holds verbatim conversation.

Nothing changes for any other origin: chat, channel, agent, cron, xin and
sub-agent principals are never owner-isolated and keep the workspace-shared view
of every chunk in the workspace, and nothing changes about the visibility a
document is ingested at. Rows written before this narrowing are not rewritten or
re-keyed; only what an owner-isolated caller may read changes, so a rollback
restores the previous visibility immediately.

Neither `document_ingest` nor `document_sync` is on the default MCP surface.
`document_ingest` is on the owner-isolation-aware list, so an operator can
expose it, and for an owner-isolated caller it confines what it writes to that
caller: it refuses a workspace `path` (the host workspace belongs to the
operator, not to an external owner) before any file is read, stores the
document `private` and refuses any other `visibility`, and stores it under
`owner-doc:{first 32 hex digits of sha256(owner id)}:{id}`, where `{id}` is the
caller's `document_id` or the `source:{sha256(source_uri)}` id. Both adapters
upsert on the document id and move the row to the writer, so without that
namespace a second caller re-ingesting the same `source_uri` replaced the
first caller's document, deleted its chunks and took it over; with it, two
callers never share an id, and one caller re-ingesting its own URI still
updates its own document. The returned record leaves out `workspace_id` (the
adapter's workspace id is the host workspace path); the path is not secret from
an MCP caller, though, because its own owner id is composed with it. `document_sync` always reads a host path, so it
stays off the list and external callers are refused it even when it is
exposed. In-process ingestion is unchanged — global `source:{sha256}` ids,
explicit visibility and `path` support — except that the `owner-doc:` id
namespace is reserved: a writer that is not owner-isolated is refused a
`document_id` starting with `owner-doc:`, so it cannot take over an MCP
caller's document by naming its id.

**`memory.acl_enabled` does not gate this.** That flag is an in-process rollout
switch: it chooses between enforcing and merely observing the memory ACL for the
agent's *own* turns. It cannot decide whether an externally authenticated caller
is scoped at all, so the three memory tools enforce the owner boundary for an
owner-isolated origin with the flag in either position, and every memory backend
enforces an owner-isolated context even when an unrestricted read is requested of
it. `memory_get`'s workspace file fallback (`MEMORY.md`, `memory/*.md`) is
**not** offered to an owner-isolated caller: those files carry no owner id, so
they cannot be owner-scoped, and the call answers with the ordinary
`No memory entry found for key` message. In-process turns keep the fallback
unchanged.

This is a visible narrowing for anyone relying on the previous behaviour. At the
default configuration (`memory.acl_enabled = false`) an authenticated MCP caller
used to read the whole memory table; it now reads public rows plus rows whose
`owner_id` matches its authenticated owner exactly. Most rows in an existing
deployment were authored by chat or channel turns and carry a channel-derived
owner id, not `owner:{workspace}:agent:{issuer}:{subject}`, so they will not
match. To share a memory with MCP callers deliberately, give it `public`
visibility rather than relying on an unowned read.

Caller-supplied `_zc_scope`, `_zc_scope_trusted`, owner id, run id or session id
arguments cannot influence any of this. Every `_zc_*` / `_prx_*` key is dropped
from the request body at the gateway and again in the execution service before
the authoritative block is written from the authenticated envelope, so a forged
scope can neither widen a read nor clear the isolation mark.

Where `{issuer}` and `{subject}` come from on the wire: a bearer JWT supplies
the configured `mcp_server.jwt_issuer` and the verified `sub` claim; a SPIFFE
caller supplies the issuer `spiffe` and the SPIFFE ID (from `X-Spiffe-Id`, or
from the SAN of a verified SVID in `X-Spiffe-Svid` / `X-Client-Cert-Pem`); an
mTLS caller supplies the issuer `mtls` and the certificate subject from
`X-Client-Cert-Subject` / `X-Forwarded-Client-Cert`. Those SPIFFE and mTLS
origins are headers asserted by a reverse proxy, so they are honored only when
`gateway.trust_forwarded_headers = true`. With the default `false` they are
ignored: they never mint an identity and never override a valid bearer, and a
request that carries only such a header is refused with 401 naming the setting.
An SVID in a header is included deliberately — verifying its chain proves
nothing about possession of its private key, so a copied certificate would
replay as its peer.

The owner id is `owner:{workspace}:agent:{issuer}:{subject}`, composed verbatim.
Its components are deliberately **not** escaped: real issuers and subjects
contain `:` routinely — JWT `iss` URLs, `spiffe://` subjects, the literal
`anonymous:mcp` — so escaping them would change every real caller's owner id,
which is also its idempotency scope and, now, its read boundary. Every owner id
is therefore byte-identical to what the same caller had before owner isolation
existed, and no stored binding, owner id or idempotency scope moves.

Ambiguity is excluded by construction rather than by escaping. The issuer
component is only ever one of the fixed origin names `spiffe`, `mtls` or
`anonymous`, or the single configured `mcp_server.jwt_issuer` — the JWT `iss`
claim is validated against exactly that value before an identity is derived from
it. The three fixed names share no prefix with one another, so two different
callers can only compose to the same owner id if the configured issuer is one of
those names or begins with one of them followed by `:`. Config load refuses such
a value, naming `mcp_server.jwt_issuer` in the error, so the ambiguity cannot
exist at runtime.

### Recovering unattributed rows

`message_events.owner_id` already existed; `memory_events.owner_id` is added by
an idempotent migration (SQLite schema v16, Postgres v22). Every existing row is
preserved. A row whose `owner_id` is NULL is *unattributed*: it stays readable
by every principal that is not owner-isolated, and it is never deleted, but an
owner-isolated caller does not see it. That is deliberate — the store cannot
prove such a row belongs to the caller.

No row is ever re-keyed by the upgrade itself: owner ids do not move, so a row
that already carried its caller's owner id stays readable by that caller. The
only rows an owner-isolated caller loses sight of are the ones that never had an
owner at all. If a caller needs that pre-upgrade history back, attribute it
explicitly, one session key at a time. Always run the dry run first and read the
count; the statements are parameterized and deliberately refuse to take a
wildcard.

SQLite (`<workspace>/memory/brain.db`):

```sql
-- Dry run: how many rows would be attributed, and to what.
SELECT COUNT(*) FROM message_events
 WHERE workspace_id = ?1 AND session_key = ?2 AND owner_id IS NULL;
SELECT COUNT(*) FROM memory_events
 WHERE workspace_id = ?1 AND session_key = ?2 AND owner_id IS NULL;

-- Apply, with ?3 bound to the exact owner id
-- (owner:{workspace}:agent:{issuer}:{subject}).
UPDATE message_events SET owner_id = ?3
 WHERE workspace_id = ?1 AND session_key = ?2 AND owner_id IS NULL;
UPDATE memory_events SET owner_id = ?3
 WHERE workspace_id = ?1 AND session_key = ?2 AND owner_id IS NULL;
```

Postgres (inside the configured schema; the tables are the configured memory
table name with the `_message_events` / `_memory_events` suffix):

```sql
-- Dry run.
SELECT COUNT(*) FROM <schema>.<table>_message_events
 WHERE workspace_id = $1 AND session_key = $2 AND owner_id IS NULL;
SELECT COUNT(*) FROM <schema>.<table>_memory_events
 WHERE workspace_id = $1 AND session_key = $2 AND owner_id IS NULL;

-- Apply, with $3 bound to the exact owner id.
UPDATE <schema>.<table>_message_events SET owner_id = $3
 WHERE workspace_id = $1 AND session_key = $2 AND owner_id IS NULL;
UPDATE <schema>.<table>_memory_events SET owner_id = $3
 WHERE workspace_id = $1 AND session_key = $2 AND owner_id IS NULL;
```

Both statements are bounded by one workspace and one session key and only touch
rows that are still unattributed, so re-running them is a no-op and an already
attributed row is never reassigned. There is no CLI wrapper: assigning an
authorization boundary is a deliberate operator action, and the shape that
matters — one stated session key, one stated owner id, no wildcard — is the SQL
above.

### Document ingest events carry the ingesting turn's scoping keys

A `document.ingested` row in `memory_events` inherits the document's own
visibility. The read predicate admits a `session` row by `session_key`, and an
`agent` or `private` row by `agent_id` / `persona_id`, so a row written with
NULL in those columns matches no arm and is unreadable by every principal,
including the one that wrote it. Both adapters now bind the ingesting turn's
`session_key`, `agent_id` and `persona_id` onto that row: the tool boundary
supplies the trusted scope's `chat_id` (a tool principal resolves no agent or
persona), and the agent loop supplies the turn runtime's three values. The
document, its chunks, their owner and the visibility the caller asked for are
unchanged; only the event row gains the keys, so nothing becomes visible to a
principal that could not already read a row of that visibility.

A `private` or `agent` ingest event is still unreadable to a principal that
carries no agent or persona. That is the pre-existing property of those arms for
every event type, not an ingest-specific rule, and it is unchanged here. A test
pins the `private` case specifically; no ingest path produces `agent` visibility
today, so there is nothing to pin for that arm.

The `private` arm requires the owner for **every** reader (OQ-1, closed): a
`private` event is readable only when its `owner_id` equals the reader's own,
in addition to the `agent_id` / `persona_id` / `sender` match, for an
owner-isolated caller and an in-process turn alike, on both adapters. Before
this a principal that was not owner-isolated matched `private` rows on
`agent_id` or `persona_id` alone, so an in-process turn of another owner running
under the same configured sub-agent name read the first owner's `private`
events — a private document's `document.ingested` event among them. A reader
without an owner id reads no `private` event at all. Principals built from a
runtime envelope always carry an owner id, so a turn still reads its own
owner's `private` events. The `session` arm is unchanged for a principal that
is not owner-isolated: its trust domain is the session key, and qualifying it
by owner would split one group chat's history by sender. The `agent` arm
carries no owner suffix in either adapter — harmless today, because no writer
produces `agent` visibility. The `workspace` arm is owner-qualified for an
owner-isolated caller and shared for every other reader; `global` stays shared
for every caller.

The event and the document it describes are scoped separately, and a
`session`-visibility document is the case where they differ. The document's
chunks are owner-only, while the `document.ingested` event is readable by every
principal that is not owner-isolated and shares the `session_key` — every sender
in one group chat, for instance. No chunk content is in that event, but its
document id, owner id, topic id, task id, source message event id, chunk count
and content hash are, for a document the reader cannot open.

Ingest event rows written before this change keep their NULL keys and are not
backfilled. Unlike an unattributed `owner_id`, there is nothing to recover them
from: the event payload records `owner_id`, `topic_id`, `task_id`,
`source_message_event_id`, `chunk_count` and `content_sha256`, but never the
originating session key, so no bounded `UPDATE` can derive one. A
`session`/`agent`/`private` ingest event from before the fix stays invisible
unless an operator assigns a key from an external record. The ingested document
itself is unaffected and remains searchable by its owner.

The `Agent::run_single` shell runs through the same service as of 0.8.128;
approval there is fail-closed for the same reason.
`tests/architecture_boundaries.rs` pins the inventory of production call sites
that invoke a tool directly, so a new bypass fails a test, and
`tests/tool_schema_contract.rs` sweeps the whole static tool inventory: every
published contract must accept a document derived from itself and reject one
that breaks it. The accepting half derives its example from the schema alone —
never from the validator's own complaints, which would make the assertion the
generator's exit condition — and validates it once, with no repair; a contract
the derivation cannot satisfy is pinned by name with its reason. Paired
accept/reject cases for `patternProperties`, undecidable `oneOf`, integral
floats, a root-level `if` / `then` / `else` and local `$ref` cover the semantics
no schema PRX itself publishes uses.
See [Tool Schema and Child Capability Parity](tool-schema-and-child-capability-parity.md)
for the architecture and acceptance matrix.

A tool whose executor applies a default action does not make the discriminator
a required field; the published schema carries the default and applies that
action's required fields when it is omitted. This covers `sessions_spawn`
(`spawn`), `mcp_call` (`call`), `message_send` (`send`), `subagents` (`list`)
and `managed_session` (`list`).

`skills_list` reports active and disabled skills, their origin/loading mode,
and whether each supported `SKILL.toml` tool is executable through the skill
adapter. `skill_read` resolves a skill by catalog name, then reads its
instruction file or a relative resource under that skill's own canonical root.
It never turns an arbitrary absolute path into a file-read bypass.
Markdown skill discovery reads at most the first 8 KiB for metadata; instruction
bodies are read only when `skill_read` is called. `skill_read` resolves its target
from the current metadata selection, so a workspace skill takes precedence over
a community skill of the same name even if an older catalog snapshot is cached.
Workspace skills also retain admission priority when the catalog reaches its
256-skill limit.

`skills_manage` is the approval-gated agent control plane for `create`,
`install`, `update`, `enable`, `disable`, `validate`, `sync`, and `remove`.
The CLI exposes `list`, `sync`, `install`, `update`, `enable`, `disable`, and
`remove`. Disabled workspace skills remain installed but are removed from
prompt selection and executable alias discovery.

`skill_execute` is the root execution fallback for enabled skill tools. It also
exports up to 32 deterministic `skill__<skill>__<tool>` aliases and is recorded
as the `skill` adapter by the canonical tool catalog. Supported manifest kinds
are `shell`, `script`, and `http`; shell/script scalar arguments are supplied
as `PRX_SKILL_ARG_*` environment variables, scripts are confined to their
canonical skill root, and HTTP tools reuse the public-network and domain
allowlist enforcement of `http_request`. Skill tool calls use the same policy,
approval, cancellation, idempotency, and audit execution service as native,
MCP, and WASM tools.

`document_ingest` accepts UTF-8 inline content or a workspace-relative file.
`document_sync` requires a workspace-relative file and derives a stable document
id from its canonical source path, so a later sync updates the same durable
document and chunks. Both operations are approval-gated, limited to 4 MiB, and
preserve source/hash metadata. `memory_reindex` rebuilds both Memory and Document
FTS indexes and backfills stale embeddings supported by the active backend.
Markdown sources are parsed as CommonMark blocks with nested heading paths;
lists, fenced code, tables, HTML blocks, and frontmatter remain atomic. Cold-start
Markdown hydration uses stable content anchors instead of line-number keys.

`mcp_status` performs discovery without calling a remote tool and reports each
server's transport, tool list, last refresh, and retained configuration or
discovery error. `mcp_call` also accepts `action: list|status|refresh`; its
backward-compatible default action is `call`, and omitting `action` is still a
valid call that is validated against the `call` contract. Discovery runs before the
provider-facing specs are selected. If an alias is discovered after an
entrypoint's immutable execution catalog was created, the execution service
requires a matching live backend spec before dispatching that alias through the
same policy, approval, preparation, and audit pipeline. MCP discovery retains the complete alias catalog without automatically
publishing aliases. The root schema stays independent of catalog size; exact
alias schemas are available to the constrained turn selector. WASM aliases
retain their bounded default surface. MCP refresh and config-reload boundaries
advance the runtime binding epoch, so an alias reference from a previous
session cannot become valid again merely because its schema and destination
later return to the same values. A decision-selected alias is checked again
after the adapter refreshes and before it dispatches to the captured server
configuration. A changed binding is refused; a new decision must select the
current capability.

Stdio MCP servers are stateful: PRX keeps one client process alive per server
and serializes that server's calls, while calls to different servers may run in
parallel. An explicit refresh or workspace MCP configuration change closes the
cached session before rediscovery. A timeout or transport failure also resets
the session, but PRX does not replay the failed call because it may have already
performed an external side effect. A cached process whose launch configuration
differs from the call's captured configuration is refused until refreshed;
a matching server name alone does not authorize reusing that process.

`wasm_plugins_manage` supports `status`, `get`, `refresh`, `install`, `update`,
`enable`, `disable`, and `remove`. Install/update sources must be directories
inside the workspace and contain `plugin.toml` plus its declared WASM file.
PRX stages and compiles the candidate before publication, atomically refreshes
the shared generation, and rolls back if any declared adapter cannot be built.
An approved managed install records a `.prx-managed` marker and grants the
plugin's declared required host interfaces; manually placed plugins retain the
conservative permission filter.
Actions that target an installed plugin or exported adapter declare `name` as
required in the tool schema. Storage store/forget actions additionally declare
their required key/content fields, so native tool callers can validate an
operation before dispatch instead of learning the contract from a failed call.
Disable keeps the plugin under `.plugins-disabled`; remove moves it under
`.plugins-trash` and reports the recovery path. Status includes active,
disabled, and isolated adapter errors. The same runtime and controls are
registered in chat, agent, gateway, and channel entrypoints.

For prompt-guided models, the textual tool catalog now uses the same per-turn
decision-selected snapshot as native function calling. Registered capabilities whose
availability is only `declared` remain visible to control-plane catalogs but are
not advertised as executable ToolSpecs.

`http_request` only accepts public HTTP(S) targets. It blocks local, private,
link-local, multicast, reserved, and legacy numeric-IP forms; applies
`browser.allowed_domains` when configured; never follows redirects; and stops
reading the response body at `http_request.max_response_size` rather than
buffering an unlimited body before truncation.

`cron` schedules with `kind: "at"` are one-shot regardless of physical retention: after their final success or failure they expose a typed terminal state and are never due again. `delete_after_run` atomically removes the job with its successful terminal commit; failures remain visible for run and event audit. The cron tool's update action can re-arm a retained terminal job with a new future `at` schedule; setting `enabled: true` alone does not, and an in-flight `at` schedule cannot be replaced. Manual `run` remains available for paused or terminal jobs; only a nonterminal `at` is consumed into terminal state. The CLI supports creating and displaying `at` jobs but does not expose an `at`-schedule update flag.

`xin` task actions are `list`, `get`, `add`, `update`, `remove`, `pause`,
`resume`, `cancel`, `run`, `runs`, `events`, and `status`. Durable workflow
actions are `goal_list`, `goal_get`, `goal_add`, `goal_pause`, `goal_resume`,
`goal_cancel`, `goal_remove`, `step_list`, `step_get`, `step_add`, and
`step_retry`. `pause` prevents future scheduling but lets a currently leased
execution finish; `cancel` revokes the lease and cooperatively stops active
work. Completed user and agent tasks remain available for `get`, `runs`, and
`events` until explicitly removed. When a trusted channel owner scope is
present, reads and mutations are restricted to that owner; local operator calls
without a channel scope retain workspace-wide visibility.

When `message_send` uses a configured channel directly, its decision-scoped
approval binds the resolved channel and recipient, as well as the original call
arguments. An implicit destination
must come from the current turn's routing context. Without that context, supply
both `channel` and `target`; mutable defaults from another turn cannot establish
approval scope. Resume resolves the destination again and refuses a changed
operation. Outbound policy and media restrictions still apply at execution.

`message_send` normally replies on the channel the turn arrived on. An optional
`channel` argument names a different configured channel instead — resolved
against the same channel registry `sessions_spawn` announces into, so an
unknown name is an error that lists what is addressable rather than a silent
reply on the current channel. Cross-channel delivery must be permitted by
`send_allow` (see [Scope rules](configuration.md#scope-rules); the default for a
destination channel other than the turn's own is **deny**) and carries **text
only**: `[IMAGE:]` / `[VOICE:]` / `[DOCUMENT:]` markers and `as_voice` are
refused, because an attachment is a local path owned by the originating channel.
`action="react"` cannot be redirected — reactions are always delivered by the
Signal handle — so it rejects a `channel` naming anything else.

In `prx chat` the same public tool name is registered with an entrypoint-specific
schema that exposes only the daemon-supported send operation and requires
`channel`, `target`, and `message`. The send is performed by the daemon: a chat session opens no IM connection of its
own, because a second listener would race the daemon for inbound messages. The
local side-effect gate checks the exact requested channel and recipient before
contacting the daemon. Under supervised autonomy, this requires a runtime
approval grant; a decision-scoped grant also requires its durable consumed-call
scope. The daemon independently applies its outbound policy and media rules.
The chat variant therefore requires `channel` (there is no conversation to inherit
one from), offers `action="send"` only, and refuses arguments it cannot carry
(`quote_timestamp` / `quote_author`) instead of dropping them. It reaches the
daemon over `POST /api/channels/{name}/send` using `[chat.daemon]`
(see [Configuration](configuration.md#chatdaemon)); when no daemon is running
the tool reports that and the chat turn continues. Every gate above — unknown
channel, `send_allow`, text-only — is applied **by the daemon**, against the
daemon's configuration. Local approval cannot override those checks. Because such a send has
no inbound conversation behind it, it is authorized as a send *from* the
operator plane channel `api`, which makes it cross-channel by construction and
therefore denied until an operator opts in with `send_allow`.

`sessions_spawn` also carries the two actions that let a correspondent on a
messaging channel hand work to a live `prx chat` session, rather than to a
sub-agent. `action="chat_sessions"` lists the sessions currently registered with
the daemon, and `action="chat_assign"` hands one of them a `task` with a
`disposition` of `queue` (default — wait for the session to be free), `steer`
(hand it to the turn running right now as extra input) or `interrupt` (stop that
turn first). Both are actions on this tool rather than tools of their own, so the
model's tool surface does not grow.

Assignment is **default-deny** and the identity it is judged on is the one the
runtime injected for the message being processed, never anything the model wrote:
an operator opts a correspondent in with `autonomy.scopes.assign_owners` or an
`assign_allow` scope rule (see
[Scope rules](configuration.md#scope-rules)). A caller with no grant is refused,
and the refusal names both parties by audit fingerprint only; the listing is
filtered to what that caller could actually assign to, so it cannot be used to
discover which sessions exist. The call is deliberately made in-process rather
than over `POST /api/chat-sessions/{id}/assign`: an HTTP request carries no
trustworthy caller identity, so the same call over the loopback API would be
authorized as the operator plane whoever typed the message.

`chat_assign` returns as soon as the mailbox accepts the task — a chat session has
no deadline, so waiting would park the correspondent's turn for as long as the
other end takes. Instead a **relay** is started: it waits for that assignment's
result and sends it back to the conversation the assignment came from, on the
channel that conversation arrived on. The destination is fixed from the assigning
turn and is never model-supplied, and the send passes the same `send_allow` /
`send_deny` gate every other outbound message does — a denied recipient is
withheld and logged with the destination's fingerprint. The relay has a
work-registry row of its own (its address is `relay_work_id` in the answer), so
`prx tasks list` shows it while it waits and `prx tasks kill` ends it; nothing
about it expires on a clock.

An accepted assignment is **durable**: the mailbox is written through to
`{workspace}/state/chat-mailbox.db` before `chat_assign` answers, and a daemon
restart replays it, so queued work is still queued and the chat keeps pulling
with the session id and token it already has. Retiring an assignment and
recording its result are one commit, so a failed write leaves the work
outstanding rather than leaving the assigner with neither the work nor an answer
about it; see [the mailbox section](configuration.md#chatdaemon) for the ordering
and idempotence guarantees that go with it. The relay itself is not — it lives
in the turn that started it — so a restart means the result reaches the
cross-session result feed rather than being relayed back to the conversation.

Agentic `delegate` configurations require an explicit non-empty
`allowed_tools`. A named list is intersected with the eligible parent registry;
unknown or ineligible names fail before the provider turn starts. The exclusive
wildcard form `allowed_tools = ["*"]` inherits the eligible parent tool surface
within the admitted child authority. Nested delegation still requires its own
selected capability, source binding and reduced-depth authority. An empty list
never means “inherit all.”

In the current decision-plane candidate, every delegate model turn requires an
admitted parent source, including when the decision plane is disabled. Agent
configuration, provider override permissions, resolved model/credentials and
multimodal settings come from that admission's configuration generation. Input
and terminal events use its bound memory fabric. A configuration reload does not
replace these settings halfway through an admitted child turn. These candidate
changes still require centralized behavior, recovery and performance acceptance.

A decision-issued child capability manifest may have an empty allowed set for
model-only work. This grants no tools and is valid only as a strict subset of
the parent's nonempty authority; owner, cost ceiling, catalog generation and
reduced depth remain bound by the seal. An empty child set cannot derive another
strict subset. This does not override the explicit agentic `delegate` configuration
requirements above or bypass the normal launch and execution gates.

Scheduler execution uses renewable database claim leases (`scheduler.claim_lease_secs`, default 90 seconds) and attempt fencing. A crashed worker's claim becomes recoverable at the lease expiry boundary, while an older attempt cannot record a run or overwrite the newer attempt's state. This protects cron state and run history from stale commits; it does **not** make external side effects exactly-once. Operators upgrading a shared database must coordinate shutdown of all older scheduler processes before starting lease-aware schedulers, because older binaries do not renew or honor the claim tuple.

### Scheduled job origin

A cron job, xin task or xin goal has no live caller when it runs: the turn that
created it ended long before. Each one therefore records its **origin** — who
created it — when it is created, and every agent run of it acts as that origin:

| Created from | Origin | A run of it |
|---|---|---|
| terminal UI, `prx agent`, `prx cron` / `prx xin` CLI, built-in and `HEARTBEAT.md` system tasks | local operator | `prx agent` CLI context: workspace-wide run view, `MEMORY.md` mirror |
| any other trusted surface (webhook, IM channel, gateway, Web Console, sub-agent, a job created by such a job) | that principal: channel, sender, chat type, owner, owner isolation | acts as that principal's owner: its own runs only, no mirror; owner-isolated if the creator was |
| no runtime-authored scope, or a row stored before this change | unattributed | acts as the job's stored owner, or — when it has none — as an owner of its own derived from the job id; never the operator |

A non-operator run keeps its `cron` / `xin` runtime source and a `cron:{id}` /
`xin:{id}` chat id, so it can never be mistaken for the terminal UI or the
`prx agent` CLI, whatever channel and sender its creator had. The origin is
fixed at creation. Only the job's creator (the same owner) or the local
operator can `update`, `remove`, `pause`, `resume` or `run` a cron job; the
operator doing so does not adopt it. Reads follow the same rule: `list` and
`status` show a caller only its own owner's jobs, and `get`, `runs`/`history`
and `events` answer another principal's job exactly as a missing one: an id
that names no job, or a removed job, reads as `not found` too, so a read
cannot confirm that someone else's job exists. (The manage actions do answer
differently: `update`, `remove`, `pause`, `resume` and `run` on another
principal's job are refused as created by another principal.) The local operator sees every
job, and can still read a removed job's events, which outlive the job. To give
a job the operator's authority, the operator re-creates it. xin applies the
same rule to its tasks and goals for a caller that carries an owner: it sees
and manages only its own owner's, and the local operator all of them. Unlike
cron, a trusted xin caller whose scope carries no owner is not filtered at all:
it sees and manages every task and goal. A call that bypasses the execution
service entirely (an in-process call with no trusted scope) keeps the
unrestricted behaviour it always had.

**Upgrade.** Jobs stored before this change have no recorded origin and are
treated as unattributed — they no longer run with operator authority, even if
the operator created them through `prx agent` or the terminal UI. The only
exception is xin `system` tasks and goals, which only the built-in definitions
and `HEARTBEAT.md` write. Re-create an operator job that relied on writing
`MEMORY.md` or on the workspace-wide run view. No origin is backfilled, so a
rollback drops nothing; an older binary ignores the column.

**Rollback and mixed versions.** An older binary does not know origins: it
runs **every** agent job — including ones a webhook, channel or Console user
created after the upgrade — as the local operator again, with the
workspace-wide run view and the `MEMORY.md` mirror, and shows every job to
every caller. Stop all older schedulers (`prx daemon`, and every node sharing a
PostgreSQL cron store) before starting this version, and treat a rollback as
re-opening that exposure. Jobs an older binary creates in the meantime store
no origin and come back as unattributed when this version runs again.

A non-operator run's tool calls are decided by the configured
`[autonomy]` policy for its creator's channel, sender and chat type, so
`[autonomy.scopes]` rules that deny the creator a tool (for example `shell` or
`file_write`) deny it in the job too. A scheduled run is unattended, so under
`supervised` a call that would ask for approval is refused rather than approved.
Jobs the local operator created keep the operator's policy. Creating a job from a
network surface therefore needs no separate capability. Two limits remain: a
scheduled run uses the `prx agent` tool registry, which can register tools the
creating surface does not; and a **shell** job (`job_type = "shell"`, or a xin
`shell` task) runs its command directly as the daemon's OS user with no tool
policy at all, whoever created it — deny the `cron` and `xin` tools to a scope
that must not run shell commands.

Git has a separate execution boundary. Use `git_operations` for model-driven
Git actions; PRX refuses recognized Git command strings in the `shell` tool,
skill shell/script tools, and
cron or xin shell jobs created or rewritten by an agent tool. Operator-created
cron and xin shell jobs, xin system tasks, and node `exec_shell` allowlists may
run Git with fixed program settings and hooks disabled. PRX records who last
wrote each shell command separately from the job's owner; rewriting a command
through an agent tool removes its operator-Git eligibility. Jobs stored before
this creator field existed are treated as agent-written. For SQLite stores,
`prx doctor` lists their IDs without printing their commands. PostgreSQL cron
jobs are not enumerated by doctor. An operator may vouch for a job they
created with `prx cron mark-operator <id>` or `prx xin mark-operator <id>`.
Detection of Git inside a script is best effort, and these operator surfaces
do not inspect repository-specific Git drivers, helpers or transport settings.
The Docker runtime forwards the fixed settings into the container without
placing their values in Docker command arguments.

Claim timestamps currently use each scheduler caller's UTC clock. Multi-node deployments must maintain NTP synchronization and bounded clock skew well below the configured lease interval. A schedule change is rejected while any claim is still active; once that claim has expired, an explicit update may clear the stale tuple. Manual runs of nonterminal jobs use the same renewable lease, delivery, and fenced commit path as background runs.

## Hooks System

Event-driven hooks let you extend agent behavior without modifying core code. Hooks fire shell commands (or WASM plugin callbacks) on lifecycle events.

### Events

| Event | When it fires |
|-------|--------------|
| `agent_start` | Agent loop begins a new turn |
| `agent_end` | Agent loop completes a turn |
| `llm_request` | Before sending a request to the LLM |
| `llm_response` | After receiving an LLM response |
| `tool_call_start` | Before a tool is executed |
| `tool_call` | After a tool completes |
| `turn_complete` | Full turn (LLM + tools) finished |
| `error` | Any error in the agent loop |

### Configuration

Create `hooks.json` in the workspace directory:

```json
{
  "hooks": {
    "tool_call": [
      {
        "command": "/usr/local/bin/log-tool",
        "args": ["--event", "tool_call"],
        "timeout_ms": 5000
      }
    ],
    "error": [
      {
        "command": "notify-send",
        "args": ["OpenPRX error"]
      }
    ]
  }
}
```

- `command` + `args` — executed directly, not via shell (no injection risk)
- `timeout_ms` — per-hook timeout, default 5000ms; valid range 1–300000ms
- `hooks.json` is capped at 256 KiB and hot-reloaded by content generation; an invalid candidate leaves the active generation unchanged
- payloads are capped at 4 MiB and passed through a restrictive temporary file plus optional stdin
- timeout covers stdin and process execution; timed-out children are killed and reaped, and temporary payload files are removed on every exit path
- WASM plugins with the `hook` capability receive the same lifecycle through
  canonical `prx.lifecycle.*` topics (native `agent_end` is exposed as
  `prx.lifecycle.agent_stop`). Exact topics, `prefix.*`, `*`, and legacy short
  event names are accepted in manifests.

`hooks_status` reports the live config path, validity, timeout, event coverage,
and action count. `hooks_manage` supports `status`, `refresh`, `validate`,
`replace`, `remove`, and `test`. Replacement is validated and atomically
published; removal moves the previous file to `.hooks-trash` and reports the
recovery path. Unknown or duplicate normalized event names are rejected. The
manager behind these tools is the same instance that emits the current turn's
events.

`wasm_plugins_status` and `wasm_plugins_manage {"action":"status"}` include
`hook_adapters` diagnostics with each adapter's patterns, invocation count,
last delivered event, and last error (the most recent failure, kept after later
successful deliveries). This distinguishes a parsed/loaded component from one
that has actually executed successfully. `hook_adapters`, `tool_adapters`,
`middleware_adapters`, `cron_adapters` and the new `provider_adapter_status`
and `storage_adapter_status` (beside the unchanged `provider_adapters` and
`storage_adapters` name lists) carry each adapter's recovery health — `state`
(`live`, `restarting`, `quarantined`), restart, failure, cancellation and skip
counters, and the last error — and `wasm_plugins_status` reports `Ready`
instead of `Healthy` while any adapter is quarantined.

## Webhook Receiver

Built-in HTTP webhook endpoint for receiving external events:

- HMAC-SHA256 signature verification
- Memory-backed event storage
- Route external events (GitHub, CI/CD, monitoring) into agent context

The local webhook lifecycle candidate keeps decision prediction and subsequent
turn execution under the same cleanup owner. If decision admission fails, it
drains owned accounting work before acknowledging the input's failed terminal
state. Cancellation retains the cleanup acknowledgement; an unconfirmed drain
remains an error rather than a completed turn. Runtime verification of this
candidate remains pending.

This change adds no configuration and does not relax authorization, approval or
budget checks. Rollback is a source change; no SQLite migration or stored-data
rewrite is required.

Chat supports `/approve --decision <uuid>` and `/deny --decision <uuid>` for a pending causal decision owned by the current chat. These commands record review through the signed decision-approval service; approval alone does not execute or resume the operation. Existing `/approve <seq>` and `/deny <seq>` still address child sessions. Use the canonical lowercase hyphenated decision UUID. Unknown, foreign, expired and failed reviews produce the same unconfirmed result and are not automatically retried.

Use `/approval --decision <uuid>` to inspect the pending call before review. It displays the existing owner-authorized DTO, including redacted arguments and original-call availability; large output is truncated with a marker. `/revoke --decision <uuid>` records revocation of reviewed grants and does not undo any effect already executed. Neither command invokes a provider or resumes a tool. Unknown or foreign decisions do not reveal their existence.

`/resume-decision <uuid>` explicitly requests execution of the original stored call after approval. Chat records a fresh canonical request and run, rechecks its source and pending approval, and uses the existing one-use direct execution service. Ordinary JSON chat messages do not select resume mode. The resume runs as a foreground ordered turn without a main-provider call; terminal status reflects the tool outcome, and a failed or uncertain attempt is not automatically repeated. A closed decision is rendered without running a tool.

Chat prompt Skill metadata and tool selection no longer accumulate across turns. Each turn uses its current selection in canonical prompt order; decision-owned turns continue through the pinned constrained execution reader. Legacy routing uses its stateless fallback with the existing operator exclusions. Cache stability and performance remain subject to the unchanged centralized budgets.

Pending decisions receive a display number from the same monotonic allocator as child sessions, with separate typed mappings. `/approve <seq>` and `/deny <seq>` can review these decisions through the signed service. Numbers are local to this Chat process, tied to the originating chat identity, never reassigned, and bounded to 1024 decision aliases. Inspecting an authenticated pending UUID registers its alias; UUID commands remain available when an alias cannot be allocated or after restart. Switching chats cannot redirect an old decision number to a child session. Every review still revalidates the durable record and grants.

Chat approval, rejection, inspection, revocation and explicit resume commands require local terminal operator input, including child-session `/approve` and `/deny`. The same requirement applies to `/bg`, session kill/steer (including daemon targets), shell/PTY session commands, `/workers cancel` and `/workers stop`, and plain text steering an attached session. Daemon-assigned task text cannot exercise this operator authority. These checks also apply while a provider turn is active; read-only session navigation and worker status remain available.

### Chat Plan mode and decision admission

The local candidate binds a model turn to the Chat mode captured when its real
source is admitted. Plan's existing tool-name classifier also narrows G9 and
subsequent capability admission; it does not copy the catalog into a per-turn
exclusion list. Resolved executable tool names (or skill/node execution roots)
are checked, while ordinary core memory reads and answers retain their existing
rules. Configured autonomy, approval, source and schema checks still apply.

The plain Chat runner retains the admitted mode. A queued turn whose consumer
mode differs fails before a fixed reply, provider request or tool execution; it
must be submitted again under the intended mode. A fresh approval resume retains
both its exact-call restriction and the current admitted Plan restriction;
approval does not remove Plan interception. This also applies when the decision
plane is disabled, without enabling remote prediction or decision persistence.
Explicit `/bg` remains an operator command under its existing authorization
contract and cannot be reused as a model-turn admission.

These changes are candidate behavior awaiting centralized validation, including
an approved fresh-resume end-to-end case. They do not establish release or
performance acceptance. Rollback must preserve source and terminal evidence;
reverting the decision projection leaves the existing late Plan interceptor in
place but restores the known predecision policy mismatch.
