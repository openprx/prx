# Unified turn context

PRX compiles one immutable capability snapshot for every model request. That
snapshot is the authority for what the model sees and what the runtime may
execute during that iteration.

The former monolithic Agent loop has been removed. Maintainers and internal
Rust callers should use the current owners and typed entry described in the
[Agent turn engine source migration](agent-turn-engine-migration.md); that note
also records the current repository-wide large-file inventory.

## Contract

The turn pipeline runs in this order:

1. Record the original input and obtain the source-bound turn decision. Build
   the system prompt from identity, behavior, channel context and admitted skill
   metadata; enrich the input only from the decision's selected evidence.
2. Resolve selected tools from the pinned runtime catalog with configuration,
   model allowlists, runtime availability and exact MCP/WASM definitions.
3. Apply runtime middleware, then reapply non-negotiable exposure filters.
4. Compile the final core and appended-discovery `ToolSpec` lists into
   `CompiledToolContext`. Each list is canonicalized separately. Core names
   take precedence over duplicate appended names, and the core remains the
   first segment in native arrays, prompt schemas and snapshot fingerprints.
   Per-spec hashes use the capability catalog's shared fingerprint function.
5. Render that exact snapshot either as native provider tools or as the
   prompt-guided tool protocol.
6. Execute snapshot entries only with source-bound execution bindings. A name
   returned by the live discovery port goes through separate admission; it does
   not gain authority from appearing in a tool result. Policy, approval,
   preparation, idempotency and audit checks still apply before dispatch.
7. Append `llm.request.context.compiled` to the shared `message_events` stream
   before calling the provider. The event carries session/run/causation
   lineage, the final system-prompt hash and size, and a content-free tool
   snapshot manifest with overall and per-tool hashes.

This gives every entrypoint the same safety invariant:

```text
advertised tools == request ToolSpecs == executable tools for this iteration
```

Decision-constrained Agent requests also carry execution bindings for the
exact compiled definitions. The execution service checks the source envelope,
catalog generation, schema fingerprint and live backend bindings before
policy evaluation and again after approval/preparation/reservation, before
invoking the backend. A stale binding returns `stale_capability`; a reservation
that cannot be safely abandoned returns `indeterminate` without invoking the
backend. The decision-stage deadline bounds prediction, rehearsal and hops;
it is not reused as the timeout of a subsequent provider/tool round trip.

An empty snapshot means no tools. A known tool omitted from the snapshot is not
executable, even if its backend remains registered. A genuinely unknown name
continues through the execution service's normal unknown-tool response so the
two failure modes remain observable.

Skills follow the same dependency rule. PRX must not advertise or inject a
skill when `skill_read` is excluded by global or model-specific configuration.
`skill_read` is a core prompt dependency; skill execution aliases remain
subject to normal selection and exposure rules.

## Ownership

| Concern | Owner | Rule |
|---|---|---|
| Stable agent prompt sections | `src/agent/prompt.rs` | Entrypoints supply `PromptContext`; they do not concatenate their own copies |
| Per-turn capability snapshot | `src/agent/turn_context.rs` | The only bridge from selected `ToolSpec`s to provider representation and execution allowlist |
| Prompt-guided tool syntax | `src/tools/prompt.rs` | One renderer; do not copy the protocol into an entrypoint or provider |
| Tool schemas and availability | canonical tool registry/catalog | Do not maintain a parallel description array for model exposure |
| Tool execution | `ToolExecutionService` plus the compiled exposure guard | Registered does not mean exposed |
| Skill selection | skill catalog/RAG plus `skill_read` dependency check | Do not inject unusable skills |
| Context rollover | `agent::compaction` handoff builder plus `message_events` | Cold history becomes an exact reference, never an invented summary |
| Transcript recovery | `transcript_history_lookup` | Resolve only referenced events inside the authenticated workspace/session |

Providers may delegate textual rendering to the canonical renderer for direct
provider APIs, but they must not invent another protocol or broaden the supplied
snapshot.

## Durable provenance

Request-context events are transcript metadata, not another prompt source. They
make an old turn's effective state discoverable by the existing message-event
history lookup without copying prompt text into the database:

- `system_prompt.sha256` identifies the final coalesced system message sent to
  the provider; `system_prompt.chars` supports operational comparison.
- `tool_snapshot.sha256`, `count`, `native_tools`, and ordered tool names plus
  per-spec hashes identify the exact request-local capability snapshot.
  `tool_snapshot.count` is the wire count: the specs the provider receives.
- `tool_surface` breaks the published surface down where the context budget
  measures it: `registry_roots` (root tools in the turn's registry),
  `routed_roots` (roots selected for this iteration), `published_specs` (specs
  those roots published after alias expansion and exposure filtering) and
  `by_adapter.{native, mcp_alias, skill, wasm}`. One root may publish many
  specs — a selected dynamic capability can add an exact alias schema —
  so `published_specs - routed_roots` is the number of alias specs beyond their
  roots. The block is re-taken when the switch-recovery tool is published
  after the preflight; a request-middleware rewrite of the tool list shows in
  `tool_snapshot.count` but not here.
  The chat route-decision log reports the registry root count as
  `registry_root_tools`; it is not a wire count.
- Adopted decision-constrained entries include a `decision` block with the
  issued decision ID, exact compiled snapshot ID, pinned catalog generation,
  directive kind and constraint count. `discovered_count` is zero until the
  mid-turn discovery producer is connected; it does not count catalog entries
  that were never selected. Legacy entries carry no decision block value.
- The message-event envelope supplies workspace, owner, session, run,
  causation, config-generation, agent/persona, and visibility lineage.
- `request_estimate` carries sizes and token estimates only. Its
  `budgeted_messages_tokens` and `budgeted_total_tokens` are what the
  context-budget preflight planned the request against; they are `null` only
  on a turn that runs without a compaction config. No production call site of
  the loop engine passes a literal `None` any more, which
  `tests/architecture_boundaries.rs` pins; the
  session worker budgets when its manifest carries a compaction config. The
  Gateway webhook turn (`POST /webhook` and the WhatsApp, Linq and Nextcloud
  Talk webhooks) is budgeted like the CLI, TUI, chat, IM channels and Console
  sessions. It budgets against the effective window of the model it serves
  (`[[router.models]]` or built-in `max_context` / `reserved_output_tokens`),
  as the CLI and TUI do. The native tool payload it reserves is the document
  the provider's tool-schema dialect puts on the wire, not always the
  published one: the Anthropic and Gemini cleaning inlines every `$ref`, so a
  `$defs` entry referenced three times is sent, and reserved, three times. A
  failover chain whose members declare different dialects reserves the
  published document.

The full system prompt, skill instructions, and tool descriptions/schemas are
deliberately not duplicated in `raw_payload_json`; they can contain private
workspace context and are already owned by their canonical sources.

The default `agent.compaction.mode = "switch"` follows the same ownership rule.
When the hot provider window crosses its budget, PRX proves that the cold
user/assistant messages map to an exact, contiguous set of durable
`message_events`. It then replaces only the in-memory/provider projection with
a versioned `[context_handoff]` JSON note containing a generation number,
parent-handoff lineage, the source event IDs, row range, retained-message
count, and ready-to-call arguments for `transcript_history_lookup`. It also
appends a `context.switch.created` audit event. No summary model or memory-flush
prose runs in this mode, and the source events are neither rewritten nor
deleted. A later rollover can reference the prior handoff event exactly, so
repeated switches form an auditable generation chain within the same durable
session.

When a legacy mode does recall an evicted OS-paging page, the injected message
is framed as history, not as an observation: the `[Recalled context from earlier
in this session]` marker line is followed by a notice stating that the excerpts
are a verbatim transcript of earlier turns, that every value in them describes
the state at the time it was recorded, that nothing in them may be presented as
work done in the current turn, and that a current value must be re-measured with
a fresh tool call. Each excerpt is introduced by a `[page evicted_at=<timestamp>]`
source line so the age of the excerpt is visible.

Switch mode bypasses the legacy OS-paging layer. Its recovery tool is a hard
runtime dependency: PRX restores the canonical lookup spec after decision/model
selection and request middleware, and restricted worker, spawn, and delegate
registries inherit it whenever their parent registry provides it. A missing
lookup backend fails the turn before a provider request.

If exact provenance or an authenticated durable scope is unavailable, switch
creation fails closed: PRX does not manufacture a reference or fall back to a
lossy trim. The legacy
`safeguard` and `aggressive` modes remain explicit compatibility options.
`transcript_history_lookup` rejects caller-supplied runtime scope, uses the
execution service's trusted scope, enforces memory visibility, requires the
current session, and returns every referenced event in note order or an
incomplete-reference error.

`channels::build_system_prompt_with_mode` remains as a compatibility entrypoint,
but it only translates channel inputs into the canonical `PromptContext` and
delegates to `SystemPromptBuilder`. Identity-file ordering, AIEOS fallback,
bootstrap truncation, task-mode guidance, safety, workspace, time zone, and
runtime metadata therefore have one implementation. Capability-dependent
hardware guidance is request-local and comes from the same compiled `ToolSpec`
snapshot as the tool protocol; the stable prompt never infers it from a static
entrypoint catalog.

## Making changes

When adding or changing a tool:

1. Change its canonical `ToolSpec`, tier, categories, or availability at the
   registry/backend boundary.
2. Route selection through the existing per-turn pipeline. Do not append tool
   instructions to a channel, gateway, chat, or agent prompt.
3. If middleware rewrites specs, keep the post-middleware exposure filter.
4. Add evidence for both representation modes when relevant: native tool
   registration and prompt-guided rendering.
5. Add an execution test proving that a registered tool omitted from the turn
   snapshot cannot run.

When adding or changing a skill:

1. Keep discovery and instruction loading in the skill catalog and `skill_read`.
2. Verify that each applicable entrypoint uses the same dependency check.
3. Test the disabled/excluded case as well as successful selection.

When changing context rollover:

1. Keep the durable transcript append-only; mutate only the provider-history
   projection.
2. Require exact event provenance before emitting a handoff note.
3. Put lookup arguments in the note instead of inferred natural-language state.
4. Test exact recovery, ordering, missing references, and cross-session denial.
5. Keep summary-based modes explicit and separate from the `switch` path.

## Cleanup rules

- Remove copied protocol strings after routing their callers to
  `render_prompt_guided_tool_protocol`.
- Add or modify stable prompt sections only in `src/agent/prompt.rs`; channel,
  gateway, chat, and worker code may contribute typed context, not copied prose.
- Remove static tool catalogs from stable system prompts. Human-facing docs may
  describe categories, but runtime exposure comes only from the current
  `ToolSpec` snapshot.
- Keep `TOOLS.md` for environment-specific operator notes. The shared tool loop
  renders the compact runtime capability list from the final request-local
  `ToolSpec` snapshot for both native and prompt-guided providers.
- Preserve `BOOTSTRAP.md` as the first-run record, but stop injecting it once
  `workspace/state/bootstrap.completed` exists.
- Treat compatibility wrappers as delegates, not alternate sources of truth.
- Remove transitional fields and parameters once all callers use the compiled
  context; do not silence dead code without recording why it remains.
- Keep cleanup behavior-preserving and covered by focused regression tests
  before combining it with new selection behavior.

## Review and collaboration

A change is ready for review only when the diff answers these questions:

- Which component owns the changed data?
- Can any entrypoint advertise a tool or skill that it cannot execute?
- Can a registered but unselected tool execute by naming it directly?
- Do native and prompt-guided providers receive the same names and schemas?
- Were gateway, channel, CLI/TUI, delegated-worker, and legacy paths either
  tested or explicitly marked out of scope?

Use focused unit tests while iterating, then run formatting, all-target checks,
clippy with warnings denied, and the relevant integration suite. Deployment,
release tags, and pushes are separate operational actions and require their own
authorization and end-to-end verification.

## Embedded Agent API admission

`AgentBuilder::runtime_config(config, provider_name)` supplies the complete
configuration and exact provider label for the injected provider instance.
Explicit builder overrides are merged before the configuration generation is
pinned. The security policy must use the same workspace. Dynamic MCP registries
also supply their actual metadata owner with `AgentBuilder::mcp_tool`.

`Agent::turn` and `run_single` require this binding before recording input or
calling a model. Library callers use the `agent_api` / `embedded-caller` source,
with a stable session and a fresh run for each input; they do not acquire CLI
operator identity. The shared decision service owns recall, skill publication,
compaction and tool selection. Execution and terminal settlement use that same
source and decision. The former keyword-based whole-registry fallback is removed.

Existing callers that supplied only `AgentConfig` must supply a complete runtime
configuration and matching provider label. This interface migration remains
subject to the public API regression suite and the program acceptance gates.

Embedded user-memory promotion respects `memory.auto_save`,
`memory.semantic.auto_promote_user_messages`, and `memory.semantic.min_chars`.
It stores original input under `user_msg:<run-id>` with the source session,
workspace, owner, and recorded input event when available. This replaces the
legacy single `user_msg` key and preserves separate turns. Writes require an
owner-aware backend; unsupported or failed promotion is best effort and emits
a content-free warning. Assistant replies are not automatically promoted by
this API. Disable `memory.auto_save` to stop promotion; this does not disable
input event recording. Existing legacy keys are left intact.

Migration limitation: every builder rejects an explicit custom `MemoryLoader`
at construction, including a builder without `runtime_config`. The source-bound
builder also rejects an injected `RouterEngine`. Neither callback is silently
ignored. Legacy memory callbacks receive an unscoped backend, and legacy router
callbacks do not bind the selected provider/model to the new turn source.
Their selected-retrieval and source-bound routing integrations remain open;
callers depending on them are not yet migration-ready. Do not remove these
options merely to make a dependent application pass construction.

For presentation customization, implement
`agent::memory_loader::SelectedMemoryFormatter` and supply it with
`AgentBuilder::selected_memory_formatter`. Its `format_context` receives only
summaries from the current decision's selected memory read. It receives no
memory backend and is skipped when no memory was selected. The returned string
is prompt context; it cannot alter the selected tools or source proof. Formatter
errors terminate the turn through failure settlement instead of performing a
fallback recall. The default presentation remains one summary per line.
This is a replacement for formatting responsibilities of a legacy loader;
custom retrieval behavior still requires integration with decision admission.

For non-delegated embedded turns, task-routing model hints take precedence over
query-classification rules. Selection uses original input and the pinned
configuration. Same-connection hints resolve to concrete model IDs before
source admission and provider execution; an unmatched later turn returns to
the configured base model. For another connection, register its actual instance
with `AgentBuilder::route_provider(provider_name, model_name, provider)`. The
selection must resolve to that declared provider and credential origin; duplicate
connection bindings and routes without a matching instance are refused. A
route-specific credential selection may retain its hint in the source proof,
while execution uses its resolved provider/model pair. The shared engine checks
that pair against the proof before calling the selected instance.
Injected-router escalation and automatic task delegation remain migration gaps;
delegation is explicitly rejected before input recording until it can use
admitted child execution.

An initial `hint:` model is also resolved before binding. Its destination must
match the explicitly supplied provider label and credential origin; the caller's
requested hint remains part of binding verification while execution uses the
concrete model ID. This does not create another provider instance.

A route-owned credential does not have to be duplicated in the root credential
slot, including for the initial model. If replacing a hint with a bare model
would change credential identity, the source keeps the validated route proof.
Execution still uses the resolved destination and the bound provider instance;
terminal provider outcomes record that destination rather than the primary
provider or the hint spelling.

Embedded skill instructions come from the workspace capability catalog.
Nonempty `AgentBuilder::skills` injection is rejected: supplied bodies are
neither catalog entries nor evidence that a skill was selected. Install the
skill through the workspace skill mechanism, register the actual `skill_read`
backend in the tool registry, and let capability discovery and decision
admission select it. Prompt publication uses selected metadata; instruction
bodies are loaded only by the admitted `skill_read` path. An empty legacy skills
list remains accepted. This changes the legacy eager-injection contract and
requires migration of applications that supplied skill bodies directly.

The embedded history view restores standard native assistant tool calls and
individual tool results as `ConversationMessage` variants only when converting
back through the dispatcher reproduces the exact wire role and content.
Reasoning payloads, extensions and unsupported encodings stay raw `Chat`
messages; XML and custom dispatchers keep raw history unless they implement
`ToolDispatcher::restore_history`. Each wire message retains its position, so
partial tool batches do not acquire invented results or change terminal history
lengths. Custom implementations must preserve this exact-round-trip contract.

Dropping an in-flight embedded turn writes its latest shared-loop history back
to the Agent, including records flushed by the inner tool-execution guard.
Cancellation uses raw `Chat` messages and does not invoke custom history
formatters during destruction. Normal completion still restores the dispatcher
view. This history writeback is separate from asynchronous terminal cleanup;
it does not assert that a cancelled turn completed successfully.

The same Agent waits for its previous turn's cleanup acknowledgement before
admitting another input. Cancelling that wait retains the pending confirmation.
A failed or lost acknowledgement leaves the Agent unable to admit more turns;
clearing conversation history does not bypass this boundary. Normal successful
cleanup permits the next turn. This serializes pending terminal writes for the
Agent without claiming that cancellation itself completed successfully.

The former public `tools::intent` module is removed from production builds.
Keyword selection, whole-registry fallback, and cumulative tool exposure survive
only as historical test fixtures while coverage migrates to scoped discovery.
Callers must obtain tool authority from the turn decision; there is no public
compatibility selector that can recreate the old unrestricted surface. Historical
full-registry assertions do not establish decision-plane acceptance.
