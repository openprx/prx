# Tool Schema and Child Capability Parity

Status: implementation in progress and acceptance contract
Date: 2026-09-12

## Implementation status

Completed in version 0.8.110:

- canonical schema construction, validation, retry diagnostics, and registry
  admission checks;
- action-specific schema migration for the native multi-action surfaces listed
  below;
- preflight rejection before runtime scope and approval injection;
- task/process selection parity for multiple aliases exported by one dynamic
  router;
- on-demand dynamic discovery before resolving explicit child allowlists;
- reconstruction and startup verification of general process-worker tools,
  including orchestration, messaging, image, configuration, Gateway, Skill,
  MCP, WASM, Hook, memory, network, and scheduling surfaces.

Remaining architecture work:

- seal the exact parent public capability snapshot and schema hashes into the
  worker manifest;
- add the authenticated parent broker for live parent-owned capabilities;
- compare parent, task-child, and process-child catalogs before the first model
  request;
- complete deployed fullscreen-TUI acceptance across the matrix in section 8.

## 1. Problem statement

PRX exposes native tools, Skills, MCP tools, WASM tools, hook controls, session
controls, channel actions, and runtime controls to language models. Two contracts
must hold across every agentic entrypoint:

1. A tool schema must describe every executable public operation, including the
   parameters required by the selected operation.
2. A child agent must inherit the parent's effective capabilities by default.
   Isolation may restrict authority, lifetime, workspace, or recursion depth, but
   must not silently replace the capability set with a smaller unrelated set.

The current implementation violates both contracts. Most multi-action schemas
only require the discriminator (`action` or `operation`), while executors require
additional fields after dispatch. In-process task children reference the parent's
live registry, but OS-process workers rebuild a base registry from configuration
and therefore lose entrypoint-owned tools and live handles.

## 2. Terminology

- **Tool implementation**: one Rust `Tool` object capable of executing one or
  more public tool names.
- **Public tool**: one `ToolSpec` name visible to a model. Dynamic MCP and WASM
  aliases are public tools even when one router executes them.
- **Action contract**: the discriminator value and its required parameters,
  parameter constraints, and accepted defaults.
- **Capability manifest**: the immutable, request-local inventory of public tool
  specs and execution locations available to a parent turn.
- **Local capability**: executable inside the worker from sealed configuration.
- **Parent capability**: requires a live object owned by the parent process and
  is invoked through authenticated IPC.
- **Authority**: whether the caller may perform an operation. Capability
  inheritance never bypasses security policy, approval, scope, or depth limits.

## 3. Required invariants

### 3.1 Schema invariants

1. Every advertised public tool has an object schema.
2. Every executor-required input is represented by the advertised schema.
3. Multi-action tools declare discriminator-specific requirements.
4. The same action contract drives provider advertisement and runtime preflight.
5. Runtime validation checks types, enums, nested required fields, and conditional
   requirements before invoking the executor.
6. Validation errors include the action, missing/invalid fields, expected types,
   and a retryable example shape without inventing values.
7. Dynamic MCP/WASM schemas are validated at discovery time. Missing or invalid
   upstream schemas are explicitly marked degraded and receive a safe object
   fallback; they are never treated as verified schemas.
8. Provider-specific schema projection may remove unsupported annotations but
   must preserve required semantics. If a provider cannot express conditional
   requirements, PRX still enforces the canonical contract locally.

### 3.2 Parent/child capability invariants

1. With no explicit allowlist, or with `allowed_tools = ["*"]`, a child receives
   the parent's effective public capability manifest.
2. Explicit allowlists narrow that manifest and may not broaden it.
3. `transcript_history_lookup` is added when context recovery requires it.
4. Dynamic aliases preserve public names, descriptions, schemas, and execution.
5. Task and process modes expose the same public capability names for the same
   parent turn, except capabilities explicitly declared parent-only by policy.
6. Parent-only tools are callable through a parent broker when safe; otherwise
   they appear as unavailable with a deterministic reason rather than silently
   disappearing.
7. Recursion is governed by `max_depth` and runtime lineage, not by deleting
   delegation tools from the child's catalog.
8. Security scope, approval grants, channel identity, owner/topic/task lineage,
   cancellation, and audit events cross the boundary as sealed data.
9. Tool tiering operates on the assigned task and cannot hide explicitly
   inherited orchestration dependencies. An explicit allowlist is an exposure
   decision, not merely a registry-selection hint.

## 4. Canonical schema design

PRX will add a small schema-contract layer in `src/tools/schema.rs`:

- `ActionRequirement` describes one discriminator value and required fields.
- `with_action_requirements` appends canonical JSON Schema conditional clauses.
- `validate_tool_arguments` evaluates the canonical subset used by PRX before
  executor dispatch.
- `format_argument_validation_error` produces structured, model-retryable
  diagnostics.

The supported canonical subset is deliberately small and auditable:

- object `properties`
- root and nested `required`
- scalar and union `type`
- `enum` and `const`
- `anyOf` and `oneOf`
- `allOf` containing `if.properties.<discriminator>.const|enum` plus
  `then.required`
- array `items`
- string and array lengths
- numeric bounds
- `additionalProperties: false`

Complex business validation remains in executors. The contract layer prevents
calls that are structurally impossible and keeps error recovery deterministic.

Initial migration covers every multi-action native surface:

- `xin`
- `cron`
- `sessions_spawn`
- `subagents`
- `skills_manage`
- `hooks_manage`
- `wasm_plugins_manage`
- `gateway`
- `nodes`
- `mcp`
- `composio`
- `message_send`
- `git_operations`
- TUI `managed_session`
- TUI `chat_schedule`

`proxy_config` has no executor-required action payload: `set` accepts partial
updates and the remaining actions use no additional input. It therefore needs
only its root action declaration, not a conditional requirement.

Future multi-action tools must declare action requirements in the same change
that introduces executor branches.

## 5. Capability inheritance design

### 5.1 Task mode

Task mode continues to proxy the parent's live `Arc<dyn Tool>` registry. The
selection layer preserves every public spec exported by an inherited root tool.
Explicit allowlists may name root tools or multiple dynamic aliases from the
same router. Delegation tools remain present; depth enforcement is performed
when executing them.

### 5.2 Process mode

The parent snapshots the effective public `ToolSpec` catalog into the sealed
worker manifest. Each entry records an execution location:

- `worker_local`: the worker reconstructed an equivalent public tool.
- `parent_proxy`: execution must be sent to the parent-owned tool instance.
- `unavailable`: policy deliberately refuses the capability and supplies a
  reason.

The worker compares its reconstructed local catalog with the manifest before the
first model request. Missing parent capabilities cannot be silently ignored.

Parent-proxy execution uses a bidirectional framed protocol separate from final
human-readable output. Each request carries run id, tool call id, public tool
name, arguments, sealed scope, and cancellation identity. The parent resolves
the name against the exact frozen registry used to build the manifest, applies
normal runtime validation and security gates, executes it, records ordinary tool
events, and returns a `ToolResult` frame.

The first implementation may classify inherently interactive TUI operations as
unavailable in process mode, but the mismatch must be explicit and testable.

## 6. Exposure and routing

Capability inheritance and intent-based prompt-size optimization are separate:

- inherited/default capabilities establish what the child may use;
- intent routing chooses the initial advertised subset;
- explicit task instructions and explicit allowlists force the named capability
  category into the advertised subset;
- after a structural tool error, the retry keeps the failed tool advertised;
- orchestration routing uses language-independent semantic intent signals. It
  does not depend on hard-coded phrases from any specific natural language.

## 7. Security requirements

- The worker never receives a reusable parent-wide bearer credential.
- Capability manifests are covered by the existing worker HMAC.
- Parent-proxy requests are accepted only on the private child pipe for the
  active run and only for names in that run's manifest.
- Runtime arguments supplied by the model cannot forge trusted scope or approval
  fields; the parent injects those fields after stripping untrusted copies.
- Channel sends, configuration mutation, process control, and destructive file
  operations retain their current approval and resource gates.
- Cancellation terminates both the worker call and any parent-proxied operation.

## 8. Verification matrix

### 8.1 Automated contract tests

- Enumerate every registered static and dynamic public `ToolSpec`.
- Validate canonical schema syntax and provider projections.
- For every action contract, test one valid argument object and one object
  missing each required field.
- Assert that runtime preflight refuses invalid calls before executor side
  effects.
- Assert schema/action parity for all migrated tools.

### 8.2 Parent/child parity tests

- Compare parent, task child, and process child public names and schema hashes.
- Exercise one static tool, one Skill tool, one MCP alias, one WASM alias, one
  memory tool, one Xin/cron operation, and one parent-proxied entrypoint tool.
- Verify explicit allowlists narrow both task and process modes identically.
- Verify recursive delegation is available but refused at configured depth.
- Verify scope, approvals, cancellation, audit events, and transcript recovery.

### 8.3 Live acceptance

All final agentic acceptance runs use `prx chat` TUI and record tool arguments,
tool results, observable side effects, session history, and daemon logs. Coverage
includes Skills and Office artifacts, MCP browser, WASM, different child models,
self-planning, scheduled tasks, execution, analysis, allocation, architecture,
audit, and decisions. TUI success does not substitute for configured Gateway or
IM delivery tests; credential-less channels remain explicitly unverified.

## 9. Delivery phases

1. Canonical validator and error recovery.
2. Migrate every multi-action schema and add parity tests.
3. Fix task-mode public-name inheritance and explicit exposure semantics.
4. Add capability manifest comparison for process workers.
5. Add parent tool broker for non-reconstructible capabilities.
6. Run focused tests, full locked test suite, release build, atomic deployment,
   doctor, and TUI end-to-end regression.

## 10. Rollback

Each phase is kept in a separate commit. The canonical validator can be reverted
without changing executor behavior. Process capability manifests are versioned;
workers reject unsupported versions rather than falling back. The parent broker
is additive and process mode can be returned to local-only reconstruction by
reverting its commit without affecting task mode.
