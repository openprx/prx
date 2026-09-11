# Unified turn context

PRX compiles one immutable capability snapshot for every model request. That
snapshot is the authority for what the model sees and what the runtime may
execute during that iteration.

## Contract

The turn pipeline runs in this order:

1. Build the stable system prompt: identity, behavior, channel context, memory,
   and any selected skill instructions.
2. Select tools from the canonical runtime registry using configuration, model
   allowlists, intent, runtime availability, and dynamic MCP/WASM discovery.
3. Apply runtime middleware, then reapply non-negotiable exposure filters.
4. Compile the final `ToolSpec` list into `CompiledToolContext`.
5. Render that exact snapshot either as native provider tools or as the
   prompt-guided tool protocol.
6. Allow execution only when the requested public tool name belongs to that
   snapshot. Policy, approval, preparation, idempotency, and audit checks still
   run after this exposure gate.
7. Append `llm.request.context.compiled` to the shared `message_events` stream
   before calling the provider. The event carries session/run/causation
   lineage, the final system-prompt hash and size, and a content-free tool
   snapshot manifest with overall and per-tool hashes.

This gives every entrypoint the same safety invariant:

```text
advertised tools == request ToolSpecs == executable tools for this iteration
```

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
| Context rollover | `agent::loop_` handoff builder plus `message_events` | Cold history becomes an exact reference, never an invented summary |
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
- The message-event envelope supplies workspace, owner, session, run,
  causation, config-generation, agent/persona, and visibility lineage.

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

Switch mode bypasses the legacy OS-paging layer. Its recovery tool is a hard
runtime dependency: PRX restores the canonical lookup spec after intent/model
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
