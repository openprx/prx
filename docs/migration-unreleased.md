# Upgrade actions for the unreleased candidate

This note lists what an existing installation must or may do when moving to
this unreleased tree. The source baseline recorded in this repository is
0.8.171 (`Cargo.toml` and the dated `[0.8.171]` changelog section; Cargo.toml
is still 0.8.171). The latest published GitHub release is v0.8.167; there is
no v0.8.171 tag. Installations on v0.8.167 also apply the changelog from
0.8.168 through 0.8.171 plus the unreleased rows below. The former decision plane
(`[causal_tree]`) is removed; the advisory decision engine
(`[decision_engine]`) remains. Items that need no action are listed so they are not mistaken for
omissions. Details live in the linked pages and in the `[Unreleased]` section
of the [changelog](../CHANGELOG.md).

## Actions

| Change | Action | Required |
| --- | --- | --- |
| Managed evolution model calls | Prompt generation now uses the resolved default model; fixed-text mutations are removed. Proposer and judge calls reserve and settle physical usage. Set `[self_system] evolution_model_max_micro_usd` explicitly for paid models; its default is zero. `evolution_model_max_tokens` defaults to 16,384 and zero disables calls. Local models still require valid prices. Whole-document replies are severity 4 and remain subject to human-review thresholds and existing write gates. Reverting this change restores the former fixed-text proposer and unaccounted judge behavior; no stored-data migration is required. | For installations using prompt evolution or a paid evolution judge |
| Flat built-in tool schemas | Built-in schemas no longer publish root `allOf`/`anyOf`/`oneOf`. Refresh cached schemas and handle conditional/alternative `InvalidArguments` errors before approval. The full original contract is checked with the selected dialect's existing lowering rules; Gemini stripping and executor clamps remain supported. Direct calls without a dialect enforce all original argument rules. Aliases and permission checks remain enforced. Native Anthropic and OpenRouter `anthropic/*` now reject unlowerable external/MCP schemas locally; republish those schemas with equally strict execution validation or exclude the tools with `[tool_tiering]`. Claude via other gateways needs an explicit provider/model `[[providers.tool_dialects]]` declaration with `tool_schema = "no_root_combinators"`. No stored-data migration is required; reverting the binary restores the former schema shape and its upstream compatibility limitation. | For external schema consumers and Anthropic users with external tools |
| Compatible-provider cache accounting | Compatible responses now retain cache-read/cache-write counts and use the effective `cache_read`/`cache_write` rates. Previously those detail fields were dropped and all prompt tokens used the input rate. Review existing `[cost.prices]` cache rates and budget expectations: their configured values are unchanged, but newly retained counts can change recorded costs. This affects PRX accounting, not provider invoices. | Before upgrading when using compatible providers with prompt caching |
| Matrix channel removed | The Matrix channel, the `channel-matrix` feature and the vendored Matrix SDK are gone. A `[channels_config.matrix]` section is ignored with one warning at load; delete it. Its values are not kept after the next config save. A `channel.matrix` proxy selector is ignored with a warning. Use ramflux for external messaging. | Only with a Matrix channel configured |
| Advisory decision engine | `[decision_engine]` defaults to `enabled = false`. Off means no analysis, brief, telemetry store, or outcome writes. The engine never selects tools or grants approval, and admission does not depend on it. Set `enabled = true` and restart only to opt into bounded pre-turn briefs and/or outcome telemetry. See [the decision engine](decision-engine.md). | To use the advisory engine |
| Recovery keys and database backup | SQLite recovery keys live in `.recovery_keys` beside the database. A usable backup is a consistent snapshot of that database (the runtime writes it with `VACUUM INTO`, never a live write-ahead-log copy) plus every key generation the snapshot names, published as one completed bundle. Copy that completed bundle. A database without those keys cannot recover. Keys without that database do not start a new authority. Incomplete backup directories are refused. | Before any backup or host move |
| Targeted gateway agents | `[gateway] targeted_agents` defaults to `false`. A named `agent` on `POST /api/sessions/{id}/message` then answers 409 and never runs as Main. The switch is pre-release: targeted history is not partitioned. | Only if you send a named agent field |
| Quiet CLI logs | Foreground commands (`prx agent`, `prx chat`, `prx go`, `prx doctor`, `prx status`, and other non-service commands) default to `warn` on stderr when `RUST_LOG` is unset, so stdout stays the product. `prx daemon`, `prx gateway`, and `prx channel start` keep `info`. A set `RUST_LOG` wins when it parses; an unparseable value falls back to that command default. Interactive `prx chat` writes the same default filter to `chat.log`; if that file cannot be opened, tracing stays on stderr. | If a script parsed tracing from stdout |
| Retired `[router]` continuation table | The retired continuation table under `[router]` is ignored with one warning at load; delete it. Turns keep the serving model's reply. Authenticated historical v1 replies remain readable. | Only if that table is still in the file |
| Source-tree layout | The repository root no longer ships a `task/` notes tree. Proptest shrinking seeds belong under `tests/proptest-regressions/`; a root `proptest-regressions/` directory is gitignored and is not replayed. | Only when working from a source clone |
| Known Codex model default effort | Declared Codex rows now use `medium` rather than `xhigh`. Set an explicit supported effort if the former behavior is required. Unknown rows keep the legacy `medium` verbosity / `xhigh` effort base. See [model levels and parameters](model-catalog.md). | Only to retain the former effort |
| WASM plugins off by default | Set `[plugins] enabled = true` and restart only after reviewing each plugin; install or update with `wasm_plugins_manage` and explicit `grant_permissions` to write `plugin-trust.json` beside `config.toml`. The configuration directory and `plugin-cache/` must be outside the workspace. A `.prx-managed` marker grants no trust. | To use WASM plugins |
| Plugin outbound requests | An empty `http_allowlist` now denies every URL. Use URL patterns such as `https://api.example.com/*`, not string prefixes such as `https://api.example.com*`. The SSRF guard still refuses private, loopback and other unsafe destinations. See [plugin trust and networking](plugin-developer-guide.md). | For plugins needing outbound HTTP |
| Workspace command hooks default off | Set `[security] workspace_hooks = "on"` only after reviewing workspace commands. Remove non-null `cwd` from `hooks.json`: the whole candidate file is refused. Commands now start with a cleared environment in the dedicated directory beside `config.toml`; use explicit paths and allowed `env` entries. See [workspace command hooks](configuration.md#workspace-command-hooks). | To run workspace command hooks |
| Terminal learning evidence format | Older binaries with strict terminal/trace readers cannot read the newly written `TerminalFacts`, experience records and optional `unavailable_attempts` field. Retain the newer binary and evidence for recovery; do not erase records to force a downgrade. | Before a binary downgrade |
| Legacy executable lifecycle skills | Migration marks these `ExecutableNeedsReview` and does not automatically import them. Review the bundle and re-import with `skills_manage` action `lifecycle_import_executable`, `source` and `revision_id`, in `auto` mode with a signed one-shot grant bound to that directory and complete bundle hash. Managed execution refuses an older unreviewed revision, including after rollback. The legacy `skill_execute` path refuses any lifecycle projection at `skills/<dir>/` regardless of `owner_scope`, including when unset. An invalid configured owner scope or an unreadable lifecycle store causes refusal, never legacy fallback. See [executable bundles](skill-lifecycle.md#executable-bundles). | To run legacy executables as managed skills |
| Managed skill compatibility directories | Treat `skills/<dir>/` as a projection of the active binding. Later publication, rollback and recovery overwrite local edits; recovery also resets the disabled marker to the binding's enabled state. First publication refuses different unmanaged content. `CommittedRepairRequired` means the binding already committed: retain the store and allow recovery to repair the copy. Linux uses native atomic directory exchange when supported and otherwise a warned two-rename fallback that can briefly leave the directory absent. On macOS, replacement of an existing projection never removes it first: unsupported exchange (`EINVAL`, `ENOSYS`, `ENOTSUP`) leaves the old tree in place and reports `CommittedRepairRequired` instead of renaming it aside. See [projection recovery](skill-lifecycle.md#compatibility-projection). | For lifecycle-managed skills |
| `[causal_tree]` removed (decision plane) | A configuration that still contains a `[causal_tree]` table, or any of its keys, is refused at load as an unknown configuration path. Nothing imports, warns about or strips it. Delete the table. The three turn limits now live under `[decision_engine]`: `latency_budget_ms` (default 500, 50-5000), `token_ratio_limit` (0.35, finite and greater than 0) and `max_candidates` (12, 1-32); an optional `[decision_engine.backend]` replaces `causal_tree.backend`. The other former keys (commit threshold, rehearsal, scoring weights, calibration, observability) have no replacement. See [the decision engine](decision-engine.md#configuration). | Only if the file still has `[causal_tree]` |
| Decision-plane data, approval resume and relaunch removed | `prx causal` prints one deprecation line and `/api/causal/*` answers 410; use `prx decision-engine` and `/api/decision-engine/*`. The approval resume route and worker relaunch are deleted. Nothing opens `self/causal_tree/decisions.db`, `.decision_key` or `self/child_authority/journal.db`; the process logs one warning and continues. Interrupted turns and pending approvals from before the upgrade are not recovered, and workers launched by an older build fail to start. To start clean, use a new workspace or delete the legacy directories. See [runtime key and legacy files](decision-engine.md#runtime-key-and-legacy-files). | Before upgrading an installation with pending approvals or running workers |
| Strict session-worker secret | `SESSION_WORKER_SECRET` must be exactly 32 bytes; otherwise PRX uses one persisted 32-byte file at `OPENPRX_SESSION_WORKER_SECRET_PATH` or `<config_dir>/keys/session_worker.secret` (mode 0600). Anything else refuses the worker launch; there is no fallback key. See [child sessions and delegation](decision-engine.md#child-sessions-and-delegation). | Only if you set `SESSION_WORKER_SECRET` |
| `agent.tool_dispatcher` is never read | Delete the key. It still loads and logs a deprecation warning. | No |

## Before and after

`agent.tool_dispatcher` (the wire form follows the provider's capabilities):

```toml
# before
[agent]
tool_dispatcher = "native"

# after
[agent]
```

`[causal_tree]` limits (the table is refused at load; the limits move under `[decision_engine]`):

```toml
# before
[causal_tree.policy]
max_candidates = 8
extra_latency_budget_ms = 800

# after
[decision_engine]
max_candidates = 8
latency_budget_ms = 800
```

## No action needed

- The [skill lifecycle](skill-lifecycle.md) is opt-in and defaults
  to disabled / `draft_only`. Its CLI and approval-bound agent actions manage
  proposals and revisions, including reviewed executable imports. Managed prompt
  targets use lifecycle evidence through the evolution scheduler. Explicit script
  drafts are opt-in and require a separate review of their complete hash before
  import; enabling generation does not enable publication or execution. New instruction
  skills can be staged from repeated uncovered completed root tasks and tested
  against a frozen no-skill baseline; this is not a learning claim. Uncovered
  recording stops at the 4096-experience threshold; evolution ticks prune expired
  unreferenced terminal experience under the configured retention policy.
  Unmanaged directories are not rewritten merely by enabling the lifecycle;
  managed bindings use the projection and migration rules above. Managed native
  scripts receive a cleared, allowlisted environment and joined cleanup, but
  deliberate `setsid` escape requires docker runtime containment today.
- Model levels and parameter overrides remain TOML settings committed through
  transactional configuration management. Catalog updates arrive only with the
  binary. Retired selections remain loadable; preview
  `prx models migrate-retired` before applying replacements. See the
  [release model catalog](model-catalog.md).
- Recursive delegation is on by default: an omitted `allow_delegation` gives a
  child that can spawn, bounded by depth, funding and integrity, and there is no
  operator setting. See [recursive child grants](tools.md#recursive-child-grants).
- The 2026-10-25 dependency review removed the `RUSTSEC-2026-0381` exception:
  RustSec now explicitly excludes the `webpki-roots` 0.26.11 bridge when its
  parent resolves to patched 1.0.10 or newer, as this lockfile does.
  The older `RUSTSEC-2025-0141` exception
  remains in both `deny.toml` and `.cargo/audit.toml`; the latter no longer lists
  `RUSTSEC-2024-0388`, which was only needed for the removed Matrix SDK. Raw informational warnings remain;
  see [the dependency exception](security.md#dependency-advisory-exception).
- Source-layout changes (`agent::loop_` removal, memory backend and chat module
  splits): no stored-data or configuration conversion. See
  [agent turn engine migration](agent-turn-engine-migration.md).
- Decision-plane removal and the fresh-workspace option: see
  [runtime key and legacy files](decision-engine.md#runtime-key-and-legacy-files).
- SQLite writer connections still use an unbounded busy handler. Deadline-scoped
  ingress and recovery reads install a busy/progress handler on that connection
  and restore the pool handler when the guard drops, including if handler
  install fails. Child-start writes take `BEGIN IMMEDIATE` so the WAL write
  lock is reserved before snapshot reads. Neither is a configuration key.
- Evolution targets shaped as `skills/<name>/SKILL.md` now require an enabled
  owner-scoped lifecycle and an existing managed binding. They stage lifecycle
  proposals instead of directly rewriting the compatibility projection. Existing
  unmanaged prompt targets retain their workflow. Proposal attempt records add
  an optional accounting scope ID; older records remain readable. See
  [managed skill prompts](evolution.md#managed-skill-prompts).

## Remaining lifecycle and runtime interfaces

Ordinary child monetary budgets now carve from their parent instead of each
receiving the full allowance. Small balances narrow a child to a zero monetary ceiling, so it can run
zero-price work while paid requests remain refused. No fresh money is created. Hook managers are explicitly inherited
independently of child tool exposure. Isolated `skills_manage` callers can read
and cancel only their own lifecycle proposals; shared-workspace writes stay
local-operator-only. `POST /api/sessions` creates an authenticated empty Console
conversation. Trusted WASM channels and the operator `prx plugin` artifact/trust
CLI are available behind the existing feature and opt-in setting. Optional Linux
`runtime.managed_cgroup_root` adds cgroup v2 cleanup for managed native scripts;
macOS keeps the documented process-group limitation. Model-backed memory and
document retrieval reserve a versioned 100 ms decision-overhead estimate; local
catalog discovery keeps zero model overhead. Actual paid requests still use the
physical accounting ledger. No database schema migration is introduced.
