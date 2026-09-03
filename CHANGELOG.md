# Changelog

All notable changes to OpenPRX will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.92] - 3 September 2026

### Fixed

- Resolve MCP, WASM, and skill aliases that are discovered after the unified
  execution service captures its startup catalog, requiring both current
  backend support and a live advertised spec before dispatch.

## [0.8.91] - 3 September 2026

### Fixed

- Preserve absolute installer `PATH` entries in generated launchd and systemd
  user services, so daemon-owned IM, gateway, hook, and scheduled turns can
  launch the same stdio MCP commands (`npx`, `uvx`, `bun`, and similar) as the
  interactive CLI.

## [0.8.90] - 3 September 2026

### Fixed

- Keep one ordinary main provider turn out of the TUI worker/session chrome, so
  internal command summaries such as `run npx ...` cannot appear below the
  composer; concurrent turns and explicit worker diagnostics remain visible.
- Separate transcript output from the gray status bar with a stable blank row
  and use the same neutral filled marker for completed tools and assistant text.
- Expose the live hook status/management tools in standalone `process_message`
  and process-isolated session-worker entrypoints.
- Make `allowed_tools = ["*"]` actually inherit the complete tool registry in
  task and process sub-agents, preserving dynamic MCP/WASM alias specs and
  named-call dispatch through the proxy boundary.
- Let explicit delegated allowlists select one dynamic MCP/WASM alias without
  exposing sibling aliases, and share the live Hooks/WASM control objects with
  in-process delegates across every model-running entrypoint.

## [0.8.89] - 3 September 2026

### Added

- Execute middleware, cron, provider, and storage WASM adapters through the
  current atomic plugin generation, with direct operational probes and real
  component fixtures.
- Resolve explicit `wasm:` providers and memory backends in chat, agent CLI,
  gateway, IM channels, daemon tasks, delegates, and session workers.

### Fixed

- Run all four middleware stages in the shared agent loop used by TUI, CLI,
  gateway, and IM delivery, isolating malformed plugin output.
- Publish action-specific required fields in the WASM management tool schema.

## [0.8.88] - 3 September 2026

### Fixed

- Make documented WASM hook manifests load by accepting both string event lists
  and `[[capabilities.events]]` pattern tables.
- Dispatch canonical `prx.lifecycle.*` topics to WASM hooks, support exact,
  wildcard, and legacy short-name matching, and expose invocation diagnostics.
- Align middleware, hook, and cron host registration with the canonical PDK KV
  result ABI and complete HTTP, memory, events, WebSocket, and WASI surface.
- Make the Rust `audit-hook` example build as a real component and connect live
  memory backends to non-tool adapters.

## [0.8.87] - 3 September 2026

### Added

- Add model-visible Hooks status and lifecycle management for validation,
  atomic replacement, refresh, recoverable removal, and synthetic event tests.
- Add model-visible WASM plugin install, update, enable, disable, refresh,
  inspection, and recoverable removal across chat, agent, gateway, and channel
  entrypoints.

### Fixed

- Register the shared WASM plugin runtime in `prx chat` and the runtime
  bootstrap instead of exposing plugin tools only in daemon/channel paths.
- Match the versioned WIT logging ABI's enum parameter so real plugin
  components instantiate and execute instead of appearing loaded but unusable.
- Surface isolated WASM adapter construction failures in plugin status and
  reject or roll back lifecycle changes that do not produce an operational
  adapter.
- Reuse the executing HookManager in chat and agent loops so control-plane
  diagnostics and changes address the same live hook generation.

## [0.8.86] - 3 September 2026

### Changed

- Make `/new` save the current chat and create a fresh session identity instead
  of aliasing `/clear`; `/clear` now explicitly keeps the current session.

### Fixed

- Reset the visible title, turns, context budget, token usage, child-session
  summaries, and pending interaction state when starting a new chat session.
- Defer `/new` and `/clear` behind an active provider turn so its ordered
  completion and durable save cannot cross the session boundary.

## [0.8.85] - 3 September 2026

### Fixed

- Hide cumulative provider-worker payload/token counters after all workers are
  terminal so completed turns and `/new` no longer display stale `wtok` state.

## [0.8.84] - 3 September 2026

### Fixed

- Enforce detached MCP child stderr through `rmcp`'s transport builder; its
  convenience constructor otherwise overwrote the command-level setting and
  continued leaking `npx` notices into the TUI.

## [0.8.83] - 3 September 2026

### Changed

- Align assistant and tool-call transcript hierarchy with Codex-style neutral
  filled markers, action wording, branch glyphs, and block spacing.
- Bound MCP output inserted into model history to 16K characters per call while
  retaining oversized results through the existing document reference path.

### Fixed

- Detach stdio MCP child stderr from the interactive terminal so `npx` and
  package-manager notices cannot overwrite the TUI input or status rows.
- Retry an empty model response once with a request-local corrective instruction
  and fail honestly if the retry is also empty instead of completing a false
  successful turn.
- Republish main-queue status after immediate dequeue so fast slash commands
  such as `/new` cannot leave a stale `queue:1` badge behind.

## [0.8.82] - 3 September 2026

### Fixed

- Keep one stateful stdio MCP client alive per configured server so browser and
  other session-oriented MCP tools retain context across separate calls.
- Serialize calls to the same stdio server, close cached sessions on explicit
  refresh or configuration changes, and reset failed transports without
  replaying potentially side-effecting calls.

## [0.8.81] - 3 September 2026

### Added

- Added approval-gated `skills_manage` lifecycle actions for creating,
  installing, updating, enabling, disabling, validating, synchronizing, and
  removing skills, with matching CLI update/enable/disable commands.
- Added executable `skill_execute` support and bounded dynamic aliases for
  declared shell, script, and HTTP skill tools.

### Changed

- Added a first-class `skill` tool adapter so declared skill tools use the same
  provider catalog and execution service as native, MCP, and WASM tools.
- Persist disabled workspace skills without deleting them and keep them visible
  to control-plane inventory while excluding them from prompts and execution.

## [0.8.80] - 3 September 2026

### Added

- Added catalog-backed `skills_list` and path-confined `skill_read` tools for
  workspace and lazy community skills.
- Added approval-gated `document_ingest` and stable-source `document_sync`
  tools for the durable document store.
- Added MCP status/error discovery and WASM plugin status/reload/root-call
  tools, with deterministic bounded alias exposure.

### Changed

- Extended the canonical tool catalog with backend-reported availability and a
  distinct WASM adapter kind; declared-only capabilities are no longer exposed
  to providers as executable ToolSpecs.
- Applied the same per-turn intent selection to native and prompt-guided tool
  catalogs, and refresh dynamic backends before each catalog snapshot.
- Rebuilt Document FTS together with Memory FTS during `memory_reindex`.
- Replaced line-number-based Markdown hydration and line splitting with
  CommonMark block parsing, nested heading paths, stable content anchors, and
  atomic list/code/table/frontmatter handling.

### Fixed

- Blocked SSRF/private-network targets and out-of-allowlist domains in
  `http_request`, including legacy numeric IP spellings, and bounded the body
  while streaming instead of after unlimited buffering.
- Retained MCP configuration/discovery failures for diagnostics and retried
  failed discovery with bounded backoff instead of silently treating a failed
  first attempt as initialized forever.
- Canonicalized the workspace root before applying ACL protection to Memory
  Markdown and SQLite files, closing a symlinked-temporary-directory bypass.
- Preserved dynamic specs, refresh hooks, cancellation dispatch, recipient
  routing, and runtime availability through the shared `Arc` tool adapter.

## [0.8.79] - 2 September 2026

### Added

- Completed Xin task management with editable tasks, immediate execution,
  retained run history, explicit cancellation, trusted-owner isolation, and a
  `prx xin` CLI.
- Exposed durable Goal/Step authoring and operations through both the `xin` LLM
  tool and CLI, including pause, resume, cancellation, removal, append, and
  ordered retry.

### Changed

- Retained completed user and agent Xin tasks until explicit removal so their
  execution results and run history remain inspectable.
- Active Xin task and Goal/Step leases now observe operator cancellation within
  one second and stop through the existing cooperative cancellation path.
- Cancelled active Xin tasks now retain a `cancelled` run-history record for
  post-run inspection.

## [0.8.78] - 2 September 2026

### Fixed

- Removed the five-minute wall-clock timeout from summary compaction. Slow
  local inference is now awaited and falls back to deterministic trimming only
  on a real provider error.
- Removed fixed context-overflow retry counts from Agent, Chat, Redux TUI and
  channel execution paths. Retries now continue while compaction measurably
  reduces history and stop only when no further progress is possible.
- Removed the hidden 1,000-message mid-turn trim and retired
  `agent.max_history_messages`; context retention is now governed by the
  configured token budget rather than arbitrary message counts.
- Calculated proactive compaction and OS-paging thresholds consistently from
  the usable input window after response-token reservation.
- Identified cl100k token measurements as a proxy instead of reporting them as
  the active model's real tokenizer.

## [0.8.77] - 2 September 2026

### Fixed

- Kept failed Xin tasks enabled and recurring work scheduled instead of
  silently disabling it after a persisted failure-count threshold.
- Made failed Goal/Step attempts return to the pending queue without a hidden
  retry ceiling; retry counters remain available for observability.
- Excluded Xin and cron lifecycle records from sub-agent session recovery, so
  scheduled work no longer appears as a process-like session.
- Reported whether memory hygiene actually ran or was skipped by its cadence
  gate instead of claiming completion for both outcomes.

### Changed

- Added Goal and Step state counts to `xin status`, separate from recurring
  task/loop counts.
- Removed the unused namespace due-task query that still accepted a batch
  limit, and retained legacy failure-cap database fields as ignored
  compatibility data.

## [0.8.76] - 2 September 2026

### Fixed

- Stopped Xin from registering duplicate fitness and memory-evolution schedules;
  existing persisted duplicates are disabled while their internal handlers stay
  available for backward-compatible goal recovery.
- Reported rescheduled Xin and cron work as pending instead of presenting it as
  a running, unmanageable sub-agent session.
- Changed generated core-memory Markdown backups to update by key and collapse
  stale generated duplicates, preventing repeated fitness reports from growing
  every future system prompt.

### Changed

- Removed the `xin.max_concurrent` and `xin.max_tasks` ceilings. Due work now
  starts without an arbitrary configuration cap, while leases, heartbeats,
  checkpoints and explicit cancellation continue to provide crash safety.
- Reduced the default Xin maintenance set to health checks, stale cleanup and
  memory hygiene; dedicated self-system workers remain the sole owners of
  fitness reporting and memory evolution.

## [0.8.75] - 2 September 2026

### Changed

- Removed the main-agent, chat, delegated-agent and spawned-session tool-round
  ceilings. Progressing work now continues until completion, explicit failure
  or user cancellation; the TUI reports `Step N` without a false denominator.
- Retired `agent.max_tool_iterations`, per-agent `max_iterations`, and the
  `sessions_spawn` `max_iterations` argument. Existing configuration keys are
  ignored with a migration warning instead of stopping startup.
- Removed wall-clock deadlines from delegated model calls, task-mode
  sub-agents, process-isolated session workers, and shell subprocesses. The
  `sessions_spawn.timeout_seconds` argument and the 24-hour
  `runtime.idle_hang_max_total_secs` ceiling are retired; active work now ends
  only through completion, a real error, no-progress hang detection, or an
  explicit cancellation such as `prx tasks kill`.

## [0.8.74] - 2 September 2026

### Fixed

- Preserved plain UTF-8 text while enabling terminal keyboard enhancements so
  Chinese and other IME-composed input renders correctly in the full-screen
  chat composer, including direct iTerm2 and tmux sessions.

## [0.8.73] - 2 September 2026

### Fixed

- Redirected chat tracing before configuration loads so routine startup logs
  remain in `~/.openprx/chat.log` instead of reappearing in the terminal when
  the full-screen TUI exits.

## [0.8.72] - 2 September 2026

### Fixed

- Removed the post-response `Thinking` card from the primary TUI transcript so
  each turn ends with a single `Worked for` summary and the assistant answer.
  Reasoning data remains available in the verbose transcript, and hidden
  reasoning no longer intercepts the tool-card `Tab` shortcut.

## [0.8.71] - 2 September 2026

### Changed

- Replaced the empty assistant waiting cursor with transcript-native, animated
  turn activity and elapsed time; running tools now own their spinner, completed
  turns retain a compact duration summary, and the pinned status bar no longer
  duplicates generation or turn-count state.
- Updated the main TUI composer prompt to `›` while retaining `>` for ASCII
  fallback and explicit labels for attached session targets.

## [0.8.70] - 2 September 2026

### Fixed

- Removed the OpenAI-compatible buffered-response two-minute deadline and
  stopped replaying ambiguous transport failures through the Responses API.
- Added WhatsApp typing and paused presence updates to long-running wacli turns.

## [0.8.69] - 31 August 2026

### Added

- Added an explicit wacli Message Yourself mode that accepts linked-account
  messages as owner input and uses wacli's opt-in self-send path for replies.

### Fixed

- Correlated self-chat replies with wacli's returned outbound message IDs so
  webhook echoes cannot re-enter the agent as new commands, including the race
  where an echo arrives before the send subprocess returns.

## [0.8.68] - 31 August 2026

### Fixed

- Included configured wacli channels in `prx channel list`, `prx channel doctor`,
  and `prx status`, and documented wacli in the channel command help.

## [0.8.67] - 31 August 2026

### Fixed

- Prevented healthy OpenAI-compatible SSE generations from being terminated by
  the non-streaming two-minute whole-response timeout.
- Prevented Chat and Agent streaming paths from replaying a request after any
  text, reasoning, or tool-call output has already been emitted.

## [0.8.20] - 26 July 2026

### Added

- Added pre-built-first `install.sh` and `install.ps1` entry points with exact
  version selection, SHA-256 verification, and atomic binary installation.
- Added installer and Rust toolchain contract gates to normal CI and the release
  workflow.

### Changed

- Aligned the declared, local, CI, audit, release, and Docker Rust toolchains on
  Rust 1.97.1.
- Updated the README and installation/configuration references to the real
  `prx` CLI and current release asset names.

### Fixed

- Removed host-shell ACL, command-text policy, OS sandbox, environment clearing,
  and synthetic PATH enforcement from interactive ShellTool, background shell,
  PTY, Cron shell, and Xin shell execution paths. Direct commands now inherit
  the parent environment and support normal host paths, `/dev/null`, variables,
  substitutions, pipelines, and background-process bookkeeping.
- Removed the obsolete `[autonomy.sandbox]` schema/templates and Landlock build
  feature so deleting the old switch cannot silently activate isolation.

## [0.8.12] - 17 July 2026

### Fixed

- Kept synchronous PostgreSQL Cron operations and client shutdown off Tokio
  runtime threads, preventing deployed `cron` commands and scheduler calls from
  panicking while initializing or querying the shared PostgreSQL store.
- Made every release platform required, failed artifact uploads on missing
  files, and added a 20-file completeness assertion before publication.
- Added a Windows MSVC check to normal Rust CI so Windows compile failures are
  caught before an immutable release tag is created.

### Release status

- `v0.8.11` published a complete cross-platform GitHub Release, but its Stage 5
  deployment was rolled back after the isolated PostgreSQL legacy-Cron check
  reproduced the Tokio runtime panic. Production returned to healthy 0.8.7.

## [0.8.11] - 17 July 2026

### Fixed

- Restored the Windows release build by making the OpenRC shell-quoting helper
  available to the platform-independent script renderer.
- Prevented the release job from publishing incomplete assets when a required
  platform build fails.

### Release status

- `v0.8.10` was stopped before deployment after its Windows build failed. Its
  incomplete GitHub Release was removed and its immutable tag retained. Upgrade
  directly from the deployed 0.8.7 baseline to 0.8.12.
- `v0.8.11` was later stopped during Stage 5 and rolled back after its isolated
  PostgreSQL Cron deployment check exposed a Tokio runtime panic.

## [0.8.10] - 17 July 2026

### Fixed

- Made Cron's SQLite schema upgrade add the legacy `cron_runs.attempt_id` and
  `worker_id` columns before creating the attempt-identity index. Existing
  0.8.7 workspaces now initialize the scheduler without losing run history.

### Release status

- `v0.8.8`, `v0.8.9`, and `v0.8.10` were stopped before deployment. 0.8.9 exposed
  this legacy Cron upgrade-order defect during Stage 5 local-binary deployment;
  production was rolled back atomically and remained on 0.8.7. The 0.8.10
  Windows release build later failed, so its incomplete GitHub Release was
  removed without moving its tag. Upgrade directly from 0.8.7 to 0.8.12.

## [0.8.9] - 17 July 2026

### Fixed

- Preserved the immutable SQLite version-4 and PostgreSQL version-5 migration
  checksum anchors published by 0.8.7. New MessageEvent execution-metadata
  columns now have new migration registry versions instead of rewriting
  previously applied history, so read-only preflight and startup accept an
  existing 0.8.7 ledger.

### Release status

- `v0.8.8` was stopped before deployment after its production-ledger preflight
  found the checksum regression. Upgrade directly from 0.8.7 to 0.8.9.

## [0.8.8] - 16 July 2026

### Added

- One process-level `ConfigGenerationManager` now owns runtime configuration
  publication. Turns, messages, Cron/Xin work, webhooks, channels, and runtime
  events retain a typed, pinned configuration generation.
- Durable event, idempotency, usage/cost, process ownership, readiness, and
  recovery contracts now cover both SQLite and PostgreSQL-backed operation.
- Stage 9 runtime domains add bounded Nodes, a process-level Skill catalog,
  generation-owned Plugins/hooks, bounded media artifacts, and truthful
  provider routing/cost settlement.

### Changed

- Configuration reloads are transactional and field-aware. Snapshot-hot fields
  apply to newly admitted work, rebuild-and-swap fields publish only after
  candidate readiness, and process-only fields are reported as
  `restart_required` without falsely advancing active state.
- Chat and agent turns share authoritative tool execution, terminal commit,
  MessageEvent, approval, and usage/cost paths. Tool calls use durable
  reserve/execute/commit/replay semantics.
- Cron, Xin/Heartbeat, Channels, webhook, and self-system supervisors use
  generation fencing, readiness, rollback, and no-overlap replacement.
- `prx chat` now defaults to the fullscreen terminal UI on TTYs. Native terminal
  scrollback is no longer the chat transcript surface; use in-app scrolling while
  chatting and `/export [md|json]` to save a transcript. `PRX_TUI=0 prx chat`,
  `prx chat --plain`, and non-TTY stdin still use the plain reedline fallback.
  The previous `[chat].tui_mode` / `PRX_TUI_MODE` inline-vs-fullscreen selector
  is removed because fullscreen is now the only TUI renderer.

### Fixed

- Prevented partial terminal commits when workspace validation or usage/cost
  settlement fails; retries remain recoverable and idempotent.
- Scoped MessageEvent idempotency by workspace in both persistence backends.
- Enforced at-most-once tool execution across Chat and agent entry points.
- Made process kill, Cron leases, webhook ingestion, service status, health,
  readiness, migration diagnostics, and configured-backend selection truthful.

### Security

- **BREAKING** — `OPENPRX_APPROVAL_OVERRIDE` env unset 不再静默 auto-approve
  supervised 模式下的 tool approval。TUI 卡片 + Y/N 键盘 (T5-1 完整版) 留
  Task #11；在 UI 接通前 fail-safe deny。显式设置 `OPENPRX_APPROVAL_OVERRIDE=allow`
  可恢复旧行为；`deny` / `no` / `n` / `0` 与未设置一致拒绝。Codex S5 P0-3 反馈
  "绝不静默 auto-approve"。`autonomy_level=Full` 不走此路径，行为不变。

## [0.3.0] - 19 March 2026

### Added

- **Xin (心) autonomous task engine** — Configuration-driven heartbeat
  scheduler for system-level autonomous work.
  - 5 built-in system tasks: health check, stale cleanup, memory
    evolution, fitness report, memory hygiene
  - 3 execution modes: Internal (Rust fn), AgentSession (LLM),
    Shell (command)
  - SQLite-backed task persistence with execution history
    (`xin/tasks.db`)
  - LLM tool (`xin`) with 7 actions: list, add, get, remove, status,
    pause, resume
  - Configurable: interval, max_concurrent, max_tasks, stale_timeout,
    builtin_tasks
  - Evolution/fitness integration mode — xin can take over standalone
    schedulers
  - Supervisor with exponential backoff restart and health monitoring
- **Chat module** — Extracted conversational session management with
  named constants
- **Terminal channel** — Dedicated terminal-based messaging channel
- `SecretStore::decrypt_and_migrate()` — Auto-migrate legacy `enc:`
  to `enc2:` (ChaCha20-Poly1305 AEAD)
- `SecretStore::needs_migration()` / `is_secure_encrypted()` — Secret
  format detection
- **Telegram mention_only mode** — Bot only responds to @-mentions
  in group chats

### Security

- **26-finding comprehensive audit** — Full regression audit of 170K+
  LOC, all findings fixed:
  - (C-1) SQLite foreign keys enabled in memory backend
  - (C-2) Cron atomic job claiming prevents double-execution
  - (C-3) SSRF DNS rebinding defense with resolved IP validation
  - (H-1..H-8) Memory LRU eviction, content hash expansion (128-bit),
    tool argument schema validation, MCP debug log redaction, rate
    limiting for web_fetch/http_request
  - (M-1..M-11) Optimistic concurrency for xin/cron stores, magic
    number constants, flaky test serialization
  - (L-1..L-4) Code quality improvements
- **Web console hardening** — 9 additional fixes:
  - (C-1) Rate limiter time arithmetic safety
  - (C-2) Config dual-store atomic update (Mutex + ArcSwap)
  - (C-3) Upload path traversal defense — reject absolute paths
    and `..` components
  - (H-1) Auth middleware now supports cookie authentication with
    CSRF protection
  - (H-2) Skill install URL validation — strict host parsing
    prevents prefix bypass
  - (H-3) WebSocket log stream connection limit (max 64 concurrent)
  - (M-1) Extended sensitive key detection patterns
  - (M-3) Pagination clamp allows small page sizes
  - (L-1) API error responses no longer leak internal Rust error
    details
- **Legacy XOR cipher migration**: `enc:` prefix deprecated,
  auto-migrated to `enc2:`

### Fixed

- **Flaky proxy cache test** — Added `Mutex` serialization to prevent
  global cache race condition
- **Onboarding channel menu** — Enum-backed selector instead of
  hard-coded numeric match arms
- **OpenAI native tool spec** — Owned serializable structs for tool
  schema validation
- **Router audit fixes** — Provider reachability filtering, lock-safe
  async persistence, reserved `router/` namespace

### Deprecated

- `enc:` prefix for encrypted secrets — Use `enc2:`
  (ChaCha20-Poly1305) instead

## [0.2.1] - 11 March 2026

### Added

- **LLM Router Phase 1-5** — Delivered heuristic routing, capability
  registry, feedback loop updates, KNN semantic routing, and Automix
  adaptive escalation.

### Fixed

- **Router audit fixes** — Applied critical/high audit hardening for
  provider reachability filtering, lock-safe async outcome persistence,
  and reserved `router/` namespace enforcement.

## [0.1.0] - 13 February 2026

### Added

- **Core Architecture**: Trait-based pluggable system for Provider,
  Channel, Observer, RuntimeAdapter, Tool
- **Provider**: OpenRouter implementation (access Claude, GPT-4,
  Llama, Gemini via single API)
- **Channels**: CLI channel with interactive and single-message modes
- **Observability**: NoopObserver (zero overhead), LogObserver
  (tracing), MultiObserver (fan-out)
- **Security**: Workspace sandboxing, command allowlisting, path
  traversal blocking, autonomy levels (ReadOnly/Supervised/Full),
  rate limiting
- **Tools**: Shell (sandboxed), FileRead (path-checked), FileWrite
  (path-checked)
- **Memory (Brain)**: SQLite persistent backend (searchable, survives
  restarts), Markdown backend (plain files, human-readable)
- **Heartbeat Engine**: Periodic task execution from HEARTBEAT.md
- **Runtime**: Native adapter for Mac/Linux/Raspberry Pi
- **Config**: TOML-based configuration with sensible defaults
- **Onboarding**: Interactive CLI wizard with workspace scaffolding
- **CLI Commands**: agent, gateway, status, cron, channel, tools,
  onboard
- **CI/CD**: GitHub Actions with cross-platform builds (Linux, macOS
  Intel/ARM, Windows)
- **Tests**: 159 inline tests covering all modules and edge cases
- **Binary**: 3.1MB optimized release build (includes bundled SQLite)

### Security

- Path traversal attack prevention
- Command injection blocking
- Workspace escape prevention
- Forbidden system path protection (`/etc`, `/root`, `~/.ssh`)
