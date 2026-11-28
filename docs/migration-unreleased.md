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
| System prompts of named agents and delegated children | One builder (`src/context/layout.rs`) now produces the system prompt of every surface. A named agent (a Console or rf session targeted at an agent, and a cron or Xin job created by one) used to receive only its identity text; it now receives the standard sections (task, context-handoff rule, safety at the agent's own effective level, orchestration, skills index, workspace, time zone, runtime) with its own identity in place of the main persona, so its prompt is about 3,000 characters longer plus the skills index. Task, `delegate` and worker-process children already had the standard sections; their prompt text is unchanged except for whitespace (two newlines fewer before the delegated section and a trailing blank line). Terminal chat, Console and webhook prompts for the main agent are byte-identical. Cached prompt prefixes of named agents and children are rebuilt once. No configuration or stored-data change is required. | None |
| Targeted gateway agents | `[gateway] targeted_agents` defaults to `true`: a named `agent` on `POST /api/sessions/{id}/message`, and a session message that wakes a named agent, now run as that agent with its own partitioned history. An explicit `targeted_agents = false` written before the one-time defaults reset (a config directory with no `.defaults-migration` marker) is treated as the old scaffolded default and reset to `true` on the first start; set it to `false` after that start, and restart, to refuse named targets again: a configured agent then answers 409 `agent_targeting_disabled` and an unknown one 404 `agent_unknown`. The value read back by `GET /api/gateway/targeted-agents` reports `restart_required` only while the value on disk differs from the running one, so writing `true` over the default changes nothing. A config that omits the key now accepts named targets. | Only to refuse named targets |
| Named agents are agentic by default | `[agents.<name>] agentic` defaults to `true`, and an agentic agent with an empty or omitted `allowed_tools` means all tools (the same as `["*"]`); such a config now loads. An existing config that omits `agentic` now gives its named agents tools: targeted turns, `delegate` and `sessions_spawn` runs with that agent get every tool their level allows instead of answering without tools. An explicit `agentic = false` keeps the answer-only, no-tools behavior. `PUT /api/agents/{name}` is unchanged (`tools` required, `agentic` is set from `tools`; `"tools": []` still yields a non-agentic agent). | Only to keep a named agent answer-only: set `agentic = false` |
| `prx agent` removed | `prx agent` is gone (clap reports an unknown subcommand). Use `prx chat --message <text>` (`-q`): the reply alone on stdout, exit 0, 2 on a policy refusal, 1 on failure; the turn is not saved unless `--save` or `--session` is given; `--workdir` moved to `prx chat`. Scripts and cron commands that call `prx agent` must be updated. | Only if you script `prx agent` |
| Outbound disclosure | `prx doctor models` probes only configured providers (`--all-providers` restores the wide probe). `prx go -k` no longer sends an unrecognised key to OpenRouter (`sk-or-` now selects OpenRouter; other prefixes need `--provider`). Skill discovery ignores `GITHUB_TOKEN`; set `[skills] github_token` to authenticate. `[skills] sync_repositories` lists the repositories sync contacts (default unchanged; `[]` disables). `prx models refresh` and `prx doctor models` contact no host unless a provider is configured, meaning it has a credential or needs none (`default_provider` as written by `prx init` without a credential does not count; see [outbound connections](outbound-connections.md#what-configured-means)); they print one line saying so instead of probing OpenRouter. New `[agent.prompt] share_host_identity` (default `true`, prompt unchanged) and `[providers] app_identification` (default `true`; `custom:` OpenAI-compatible endpoints now also send `HTTP-Referer` and `X-Title`), `[providers.endpoint_headers]`. See [outbound connections](outbound-connections.md). | Only to use the new keys, or if a script relied on the old defaults |
| Quiet CLI logs | Foreground commands (`prx chat`, `prx go`, `prx doctor`, `prx status`, and other non-service commands) default to `warn` on stderr when `RUST_LOG` is unset, so stdout stays the product. `prx daemon`, `prx gateway`, and `prx channel start` keep `info`. A set `RUST_LOG` wins when it parses; an unparseable value falls back to that command default. Interactive `prx chat` writes the same default filter to `chat.log`; if that file cannot be opened, tracing stays on stderr. | If a script parsed tracing from stdout |
| MCP HTTP headers and persistent HTTP connection | New optional `[mcp.servers.<name>.headers]` table on HTTP servers (default empty; values encrypted at rest like `env`). An HTTP MCP server keeps one connection between calls instead of reconnecting for each call. No stored-data migration. | Only to send headers to an HTTP MCP server |
| Retired `[router]` continuation table | The retired continuation table under `[router]` is ignored with one warning at load; delete it. Turns keep the serving model's reply. Authenticated historical v1 replies remain readable. | Only if that table is still in the file |
| Source-tree layout | The repository root no longer ships a `task/` notes tree. Proptest shrinking seeds belong under `tests/proptest-regressions/`; a root `proptest-regressions/` directory is gitignored and is not replayed. | Only when working from a source clone |
| Sealed record id of keyless message rows | An encrypted-scope message row without an idempotency key is now sealed under `event:{event_id}` instead of the constant `event`. Ciphertext of a keyless row written from this version on is bound to its own row and does not open in another row; ciphertext of keyless rows written before still opens in any keyless row of the same account and conversation, until those rows are resealed (a later change). Rows written before stay readable and there is no migration in this version. | No |
| Known Codex model default effort | Declared Codex rows now use `medium` rather than `xhigh`. Set an explicit supported effort if the former behavior is required. Unknown rows keep the legacy `medium` verbosity / `xhigh` effort base. See [model levels and parameters](model-catalog.md). | Only to retain the former effort |
| No spend ceiling by default | `[autonomy] max_cost_per_day_cents`, `[cost] daily_limit_usd` and `[cost] monthly_limit_usd` are now optional and unset by default. Before: a config that omitted them got 500 cents per day, 10.0 USD per day and 100.0 USD per month. Now: a config that omits them has no limit, so no spend is refused for a budget reason, no budget warning or "budget exhausted" notice is printed, and children and worker processes get the maximum allowance (`u32::MAX` cents in the signed manifest). A config that sets any of these keys keeps exactly that limit. `prx init` no longer writes `max_cost_per_day_cents`. Usage accounting and the cost journal are unchanged. To keep the old ceilings, write them into the config. Per-agent `limits` were already unset by default. No stored-data migration is required. | Only to keep the former ceilings: set the three keys explicitly |
| Nothing restricted by default on a fresh install | `[tool_tiering] channel_exclude` now defaults to an empty list (before: 14 operations tools such as `gateway`, `config_reload`, `nodes`, `skills_manage` were hidden from IM channel turns); a config that omits it now exposes those tools on channel turns, and a config that lists names keeps hiding them. `prx init --spec server` no longer writes `workspace_only`, `forbidden_paths` or `acknowledge_unrestricted_profile` (the built-in values are unchanged and have no effect at level `full`); `--spec full` no longer writes `acknowledge_unrestricted_profile`. `prx doctor` no longer warns for an explicit unrestricted profile; it prints one neutral autonomy-level line, and `acknowledge_unrestricted_profile` is accepted and ignored. Gateway pairing, channel sender allowlists, secret protection and integrity checks are unchanged. | Only to keep the old channel tool hiding: list the names in `channel_exclude` |
| Every tool published by default | The model is now offered every available tool directly on every surface (terminal chat, gateway, channels, scheduled runs, children), in a stable name order. Before: only `file_read`, `file_write`, `file_edit`, `shell` and a few session tools were offered and the rest sat behind `capability_search`, so `cron`, `sessions_spawn` and `skills_manage` went unused unless named in `[tool_tiering] always_include`. The request carries more tool definitions, so each request uses more of the context window. `always_exclude` and `channel_exclude` still hide names. When the definitions would take more than a quarter of the model's context window (an 8k or 32k window), that turn defers the tools as `defer_unlisted = true` does, with a name index on `capability_search`; `defer_unlisted = false` turns that off. | Only to keep the old deferral: set `[tool_tiering] defer_unlisted = true`; the core tools, `always_include` names and `capability_search` then work as before, and a deferred tool can also be called by its exact name |
| Remote image fetch on by default | `[multimodal] allow_remote_fetch` now defaults to `true` (before: `false`). A config that omits it now lets `http`/`https` image references be fetched into the model context; an explicit `false` keeps the old behavior. The fetch keeps the same private-address handling as `web_fetch`: below Full private, loopback and link-local hosts are refused, at Full they are reachable. | Only to keep remote fetch off: set `allow_remote_fetch = false` |
| Node agent allows any command by default | `[nodes.server] allowed_commands` now defaults to an empty list, which allows any command (before: `["echo"]`, so every other command was refused). A config that omits the key now lets an authenticated caller run any command; a config that lists commands keeps that allowlist. Authentication is unchanged: a non-empty bearer token is required at startup, `hmac_secret` is checked when set, `tls_required = true` still demands TLS on non-loopback binds, and the default bind is loopback. `blocked_commands` is still enforced. | Only to keep the old restriction: set `allowed_commands = ["echo"]` |
| Autonomy path keys unset by default | `[autonomy] workspace_only` now defaults to `false` (before: `true`) and `forbidden_paths` to an empty list (before: 18 system paths). A config that omits them gets no workspace confinement and no path denylist; a config that sets them keeps exactly what it sets. Both keys are read below level `full` only (file tools, shell path arguments, image and document tools, the TUI file picker); at `full` they were already ignored. An install that runs `supervised` or `readonly` and relied on the old defaults must now write `workspace_only = true` and its `forbidden_paths`. Owner-private file operations stay confined to the workspace at every level, independent of these keys. `prx init` never writes these keys or `acknowledge_unrestricted_profile` (the latter is accepted from an existing config and ignored, and is no longer serialised). `prx status` prints a `Cost limits:` line (any configured `[cost]` or `[autonomy]` limit, or `no limit set`) instead of `Max cost/day:`, and prints `Workspace only:` and `Forbidden paths:` only when they are set. | Only for `supervised` or `readonly` installs: set `workspace_only` and `forbidden_paths` explicitly |
| WASM plugins off by default | Set `[plugins] enabled = true` and restart only after reviewing each plugin; install or update with `wasm_plugins_manage` and explicit `grant_permissions` to write `plugin-trust.json` beside `config.toml`. The configuration directory and `plugin-cache/` must be outside the workspace. A `.prx-managed` marker grants no trust. | To use WASM plugins |
| Plugin outbound requests | An empty `http_allowlist` now denies every URL. Use URL patterns such as `https://api.example.com/*`, not string prefixes such as `https://api.example.com*`. The SSRF guard still refuses private, loopback and other unsafe destinations. See [plugin trust and networking](plugin-developer-guide.md). | For plugins needing outbound HTTP |
| Workspace command hooks run by default | `[security] workspace_hooks` now defaults to `on` (before: `off`): a `hooks.json` in the workspace runs on an install that omits the key. A config that sets the key keeps its value; `prx init` never wrote it, so nothing is reset. A hook's `env` may now set any variable (loader, interpreter, search-path, configuration-lookup, certificate and proxy variables pass through; a NUL byte is still dropped) and a `cwd` field is accepted (absolute, or relative to the workspace) instead of invalidating the file. Hooks still start from a cleared environment, so no provider key of PRX reaches a hook unless its `env` names it, and still start in the dedicated directory beside `config.toml` when no `cwd` is given. Changing hooks no longer needs a confirmation at level `full`, and enabling `workspace_hooks` is no longer a high-risk configuration change; below `full` the side-effect gate is unchanged. A child session delivers to the same hooks as its parent. See [workspace command hooks](configuration.md#workspace-command-hooks). | Only to keep hooks from running: set `workspace_hooks = "off"` |
| Terminal learning evidence format | Older binaries with strict terminal/trace readers cannot read the newly written `TerminalFacts`, experience records and optional `unavailable_attempts` field. Retain the newer binary and evidence for recovery; do not erase records to force a downgrade. | Before a binary downgrade |
| Legacy executable lifecycle skills | Migration marks these `ExecutableNeedsReview` and does not automatically import them. Review the bundle and re-import with `skills_manage` action `lifecycle_import_executable`, `source` and `revision_id`, in `auto` mode with a signed one-shot grant bound to that directory and complete bundle hash. Managed execution refuses an older unreviewed revision, including after rollback. The legacy `skill_execute` path refuses any lifecycle projection at `skills/<dir>/` regardless of `owner_scope`, including when unset. An invalid configured owner scope or an unreadable lifecycle store causes refusal, never legacy fallback. See [executable bundles](skill-lifecycle.md#executable-bundles). | To run legacy executables as managed skills |
| Managed skill compatibility directories | Treat `skills/<dir>/` as a projection of the active binding. Later publication, rollback and recovery overwrite local edits; recovery also resets the disabled marker to the binding's enabled state. First publication refuses different unmanaged content. `CommittedRepairRequired` means the binding already committed: retain the store and allow recovery to repair the copy. Linux uses native atomic directory exchange when supported and otherwise a warned two-rename fallback that can briefly leave the directory absent. On macOS, replacement of an existing projection never removes it first: unsupported exchange (`EINVAL`, `ENOSYS`, `ENOTSUP`) leaves the old tree in place and reports `CommittedRepairRequired` instead of renaming it aside. See [projection recovery](skill-lifecycle.md#compatibility-projection). | For lifecycle-managed skills |
| `[causal_tree]` removed (decision plane) | A configuration that still contains a `[causal_tree]` table, or any of its keys, is refused at load as an unknown configuration path. Nothing imports, warns about or strips it. Delete the table. The three turn limits now live under `[decision_engine]`: `latency_budget_ms` (default 500, 50-5000), `token_ratio_limit` (0.35, finite and greater than 0) and `max_candidates` (12, 1-32); an optional `[decision_engine.backend]` replaces `causal_tree.backend`. The other former keys (commit threshold, rehearsal, scoring weights, calibration, observability) have no replacement. See [the decision engine](decision-engine.md#configuration). | Only if the file still has `[causal_tree]` |
| Decision-plane data, approval resume and relaunch removed | `prx causal` prints one deprecation line and `/api/causal/*` answers 410; use `prx decision-engine` and `/api/decision-engine/*`. The approval resume route and worker relaunch are deleted. Nothing opens `self/causal_tree/decisions.db`, `.decision_key` or `self/child_authority/journal.db`; the process logs one warning and continues. Interrupted turns and pending approvals from before the upgrade are not recovered, and workers launched by an older build fail to start. To start clean, use a new workspace or delete the legacy directories. See [runtime key and legacy files](decision-engine.md#runtime-key-and-legacy-files). | Before upgrading an installation with pending approvals or running workers |
| Strict session-worker secret | `SESSION_WORKER_SECRET` must be exactly 32 bytes; otherwise PRX uses one persisted 32-byte file at `OPENPRX_SESSION_WORKER_SECRET_PATH` or `<config_dir>/keys/session_worker.secret` (mode 0600). Anything else refuses the worker launch; there is no fallback key. See [child sessions and delegation](decision-engine.md#child-sessions-and-delegation). | Only if you set `SESSION_WORKER_SECRET` |
| `agent.tool_dispatcher` is never read | Delete the key. It still loads and logs a deprecation warning. | No |
| Context compaction modes and OS paging removed | `[agent.compaction] mode` accepts `off` and `switch` (default `switch`). `safeguard` and `aggressive` load as `switch` with a warning. The OS-paging layer is gone, so `[agent.compaction.os_paging]` (and its `enabled` gate) and `agent.compaction.memory_flush` are ignored with a deprecation warning; long sessions are handed off by the context switch. Delete the keys and set `mode = "switch"` to silence the warnings. | Only if the file sets these keys or the old mode values |
| `skill_rag.top_k` removed | The prompt publishes the skill catalog whole, so `top_k` has no effect. It loads with a warning and is ignored; delete it. | Only if the file sets it |
| Message log retention key | New `memory.message_retention_days` (default `0`, which keeps every message). With `N > 0` the SQLite message log deletes runs older than `N` days, with a 15-day effective minimum for values below that. Paired rf clients must support the `turn.deleted` retention notices before the key is enabled. See [message log retention](configuration.md#message-log-retention). | Only to enable deletion |
| One-time index build at the first start | The first start after this upgrade builds two run-family indexes on `message_events` (`idx_message_events_retention_run` and `idx_message_events_retention_parent`, the same ones the retention pass creates) and runs the schema migration pass, in one write transaction. On a store of about 300,000 message rows this holds the writer for roughly 3 to 4 seconds once (about 1.7 s migration plus 1.6 s index build); other writers wait and then continue. Turns stop paying a whole-owner scan per appended event. | Start once while no turn is running on a large store |
| Transcript-index purge queue | The first start after this upgrade, in the background warm-up, creates the table `message_events_fts_purge`, replaces three triggers of the transcript full-text index and adds a fourth, in one short write transaction (a store whose index fill is unfinished gets the filling form). Deleting or blanking message rows then records their indexed text in that table instead of removing the index entries inside the same transaction, and short follow-up transactions remove them before the delete answers or at the next scrub. A crash leaves the queue in place; the next start drains it in the background. An older binary run on an upgraded store leaves the index and its triggers in a state the next start of this version detects and repairs by rebuilding the transcript index (search results are incomplete until the refill finishes). Downgrading while deletes are in progress is not supported. No configuration change. | None |

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

## One-time reset of old default values

Earlier `prx init` builds wrote every default into `config.toml` and
`config.d/*.toml`, so an installation created by one keeps the former limits
even though the built-in defaults are now unrestricted. The first start of this
build resets a value to the new default when it is exactly the old default:

| Key | Old default reset |
| --- | --- |
| `[autonomy] max_cost_per_day_cents` | `500` |
| `[cost] daily_limit_usd` | `10.0` |
| `[cost] monthly_limit_usd` | `100.0` |
| `[autonomy] workspace_only` | `true` (only when `level = "full"`) |
| `[autonomy] forbidden_paths` | the former 18-path list, exactly (only when `level = "full"`) |
| `[multimodal] allow_remote_fetch` | `false` |
| `[tool_tiering] channel_exclude` | the former 14-tool list, exactly |
| `[gateway] targeted_agents` | `false` |
| `[nodes.server] allowed_commands` | `["echo"]` |
| `[memory.events] record_tool_events` | `false` |

`acknowledge_unrestricted_profile` is no longer written when the files are
rewritten. `autonomy.level` is never touched, and a `supervised` or `readonly`
install keeps `workspace_only` and `forbidden_paths`. Authentication keys
(`require_pairing`, tokens, sender allowlists) are never touched.

- **Rule:** a value that differs from the old default, for example
  `daily_limit_usd = 3.0`, is left alone. The tree cannot tell a value `prx init`
  wrote from one the operator typed (`.init-defaults.toml` records template
  values for `prx init --force` only), so equality with the old default is the
  test. To keep a former limit after the upgrade, set it again once the reset
  has run, or set it to any other value before upgrading.
- **How:** the reset goes through the configuration transaction (one history
  record, revision check) and rewrites the configuration files in canonical form.
  A marker file `.defaults-migration` beside `config.toml` records that it ran
  (key names only) and its version, so it never runs again. A later version
  adds keys to the table: an install whose marker holds an older version resets
  only the keys added since (for example `record_tool_events`), never a key it
  already went through. One info line at start names the
  keys reset. `prx doctor` lists old default values still present.
- **Roll back:** `prx config rollback` restores the files from before the reset.
  The marker stays, so the reset is not repeated.

## Remaining lifecycle and runtime interfaces

Ordinary children and operator tasks now share their parent's budget counter
instead of each receiving half of the remaining balance, and a worker process
is sealed with the parent's whole remaining balance at launch. Every tool is
published to a child; a spend is refused only at spend time, when it would
exceed the real total kept in the workspace cost journal. A balance below one
cent gives a child a zero monetary ceiling, so it can run zero-price work while
paid requests remain refused. No fresh money is created. Hook managers are explicitly inherited
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

## Tool calls and tool results are stored

`[memory.events] record_tool_events` now defaults to `true`. With it on, every
tool call and every tool result of a turn is appended to `message_events` as a
`tool.call.recorded` / `tool.result.recorded` row (role `tool`) carrying the run,
the round and the call id in its payload. A result keeps the inline text up to
the inline limit; above it the row holds the preview and the document
reference of the stored tool output. Stored text and arguments are redacted.
These rows are log-only: they are not `message.created`, so history loaders and
the handoff match ignore them, and the gateway sync stream carries nothing for
them. Older generated templates wrote `record_tool_events = false` explicitly, so the
one-time reset above also changes that exact value to `true`; to keep recording
off after the upgrade, set it to `false` again once the reset has run. No database schema migration is introduced.

## Scheduled runs use the install's managed skills

Runs started by the install's own schedulers (Xin, cron and the heartbeat tasks) for the local operator now count as the configured `[skills_lifecycle] owner_scope` for the skill lifecycle store: `skills_manage` status and list actions and `skill_execute` of a managed skill work in those runs. Jobs created by remote principals and owner-isolated origins keep the owner-mismatch refusal. No configuration change is needed.

## Skill processes know the call's directory

Shell-kind and script-kind skill processes now get `PRX_WORKDIR` (the working directory of the calling agent) and `PRX_WORKSPACE` (its workspace root) as absolute paths. A skill that never reads them is unaffected, and the process working directory is unchanged. A managed skill call by a named agent whose level differs from the main agent's is now judged at the named agent's level for its tool grant and run identity. No configuration change is needed.

## Unmanaged skill scripts no longer inherit provider keys

The shell and script tools of a skill that is not managed by the lifecycle used to start with PRX's whole environment, provider API keys included. They now get the same environment as the `shell` tool: at an effective Full level every variable except the stripped secrets (provider credentials, `OPENPRX_*`, cloud and source-host tokens), below Full the baseline plus `[runtime] shell_env_allowlist`. A skill script that read a provider key from its environment no longer finds it; keep the credential in the skill's own configuration or, below Full, add the exact variable name to `[runtime] shell_env_allowlist`. At Full a stripped secret cannot be passed through the environment. Managed skills already ran this way.

## A changed tool registry no longer fails a running turn

When the live tool registry changes during a turn (for example `skills_manage` imports an executable skill), the turn now completes on its pinned tool surface instead of failing with `catalog and execution registry differ`. The next turn is published from the live registry, so the new skill is offered and runnable then. Worker-process and child parity checks are unchanged. No configuration change is needed.

## Webhook hook actions

A `hooks.json` action may name a `url` instead of a `command`: PRX POSTs the
hook envelope as JSON to it (optional `headers`, `timeout_ms` and
`signing_secret`; one retry; `X-Webhook-Signature` and `X-Webhook-Timestamp`
when signed). Existing hook files parse unchanged; an action with both
`command` and `url`, or neither key, is invalid. `hooks_status` gains
`webhook_count` and `webhook_urls`, the proxy service key `hook.webhook`
(selector `hook.*`) is new, and the Gateway hooks API keeps webhook fields
when it rewrites `hooks.json`. No database schema migration is introduced.

## Process-mode children use the parent's call context and keep a log

A process-mode child (`sessions_spawn` with `mode: "process"`) now receives the parent's sealed call context in its manifest: owner, level, workspace root, working directory, model and the published tool surface. The worker no longer derives them from its own configuration, so process and task children run with the same tools, directory and level. No configuration change is needed and the level a child runs at does not change.

The worker's tracing output, which was dropped before, and anything it prints to stderr are now kept in `<config dir>/logs/workers/<run id>.log` (private files, at most 4 MiB each, the newest 64 runs kept). The parent logs the path on the `process worker log` line.

The worker protocol is now version 2: a `prx` that spawns workers and the `prx` that runs them must be the same build. A mixed pair refuses to start the worker with a `protocol version mismatch` error. No database schema migration is introduced.

## Scheduled shell commands no longer inherit secrets

Cron `shell` jobs and Xin shell tasks now run with the environment of the
`shell` tool at the run's level instead of the daemon's whole environment.
At Full (the default) only the secret set is removed (provider keys, tokens,
`OPENPRX_*`); below Full the child gets the safe baseline plus
`[runtime] shell_env_allowlist`. A cron or Xin command that read a provider key
or another secret from its environment no longer finds it: pass the value in
the command itself or, below Full, list an exact non-secret name in
`shell_env_allowlist`. No database schema migration is introduced.

## Deleting a message also removes its tool and attachment rows

A message or reply delete now also tombstones, in the same transaction, the
`message.attachment.recorded`, `tool.call.recorded`, `tool.result.recorded`
and `turn.decision.claimed` rows keyed to the deleted turn (by run or by the
user event they name), and deletes the stored tool-output documents of the
run (`tool_output` and `untrusted_external`) unless a surviving tool result
row still quotes the same document. A document is not deleted when another
run's tool result references it. Memories stay unless the request carries
`forget_writes: true`. The sync stream is unchanged. No configuration change
or schema migration is needed.

## Custom endpoint key from stdin

`prx init --provider custom:<url> ... --from-stdin` now stores the key from standard input as the encrypted root `api_key`, in the same init generation as the configuration files. It used to write the files and then fail with `provider requires a catalog identity`. A key that cannot be stored now leaves no files behind for a custom endpoint; for a catalog provider whose reference store step fails, the error names the directory that was already written.

## Models without a price run while no cost limit is set

A model with no price is no longer refused when no cost limit is configured. PRX sets none by default, so a `custom:<url>` endpoint or any other model without a `[cost.prices]` entry now runs, its cost is not tracked (`unknown_pricing`), and `prx doctor`, `prx providers test` and the configuration plan report that as information instead of `G6 budget admission refuses unpriced model`. When `[cost] daily_limit_usd` or `monthly_limit_usd`, `[autonomy] max_cost_per_day_cents` or an agent's `limits.max_cost_per_day_cents` is set, the present refusal stays. To have the cost of an unpriced model counted, add `[cost.prices."<model>"]`. No configuration change is needed.

## Deleting or forgetting a message also rewrites later attachments that quoted it

The recall text a later turn attached to its user message could quote a
message that was deleted afterwards, and the stored attachment still replayed
it to the model. The attachment row (`message.attachment.recorded`) now records
the ids it was built from (payload `schema_version` 2, additive field
`sources` with `memories`, `events` and `documents`). A message or reply delete,
and `forget_writes`, now rewrite, in the same transaction, the attachments of
the session whose sources are gone: the recalled text is emptied (the row gets
`"blanked": true`, the date line stays) and the message replays as plain
input. Rows written before this change carry no `sources`; they are matched by
the text of the deleted message (at least 12 characters, first 64 compared,
same session only). Attachments that quote only a memory that still exists are
not touched by a plain delete. No schema migration, no sync-stream change and
no change to the session routes' JSON.

## Requests may be reduced later than before

The size of a request was estimated only with a local tokenizer. The provider's own count of the previous request now calibrates the estimate: the ratio of the input tokens it reported to the local count of that request scales the local count of the history and the tool surface, plus a small margin (2%, at least 16). The ratio is used only between 0.800 and 1.600, for requests of at least 500 tokens, and never with the character heuristic. A request is therefore reduced (cleared, handed off or trimmed) when it is close to the window the provider sees, not the window a proxy tokenizer sees, which is later than before on a model whose tokenizer counts fewer tokens than the proxy and earlier on one that counts more. Before the first response of a session, after a response that reported no input tokens or an unusable ratio, and whenever the system prompt, the tool surface or the model changed, the local estimate is used as before. A request that is still over the window after the reduction steps is not refused: it goes out and the provider-overflow recovery handles it, as before. The compiled-request event (`llm.request.context.compiled`) has an additive `budget` object, and a new event `llm.request.usage.reported` records the provider's count for each request next to the estimates. No configuration change is needed.

## A process child no longer fails after a configuration write

On a running gateway or chat, `prx config apply` (and any other write of the
configuration files) left every later process-mode child failing at start,
with an empty worker log, until the parent was restarted. The parent compared
the disk revision of the configuration (including its write counter) with the
revision its active generation was activated from; a write that did not change
the effective values, such as restoring the same bytes, advanced the first and
not the second. The comparison is replaced by the check that carries the
integrity: the worker must reproduce the parent's sealed effective
configuration. A change on disk that the parent has not activated (a field that
needs a restart) still refuses the child, now with the code `config_changed`.

A worker that refuses to start now says why: the refusal is one of
`manifest_invalid`, `protocol_version_mismatch`, `config_generation_mismatch`,
`config_changed` or `config_source_mismatch`; it leads the run's failure text,
is a `reason` field in the worker's and the parent's log lines, and is written
to the worker log (`logs/workers/<run id>.log`) even when the parent itself
refuses the launch after the spawn. The previous text `configuration_changed`
is now `config_changed`. No configuration change or schema migration is needed.

## A child's completion reaches the gateway session that launched it

A `sessions_spawn` child started from a Console, rf or webhook session used to
announce its completion on the gateway's announce channel, which has no
outbound transport: the announcement was only logged, and a parent told to wait
for it never received anything. The completion is now sent through the session
mailbox, from the child's session (`spawn:<run id>`) to the parent session, and
wakes the parent as one inbound message (after the running turn, if any). The
rf client sees it as it sees any adopted mailbox message from another session:
a `turn.appended` user-role row whose text starts with
`[message from spawn:<run id>/<agent>]`, followed by the woken turn's events.
No configuration change or schema migration is needed.

## Old tool results are cleared first on every surface, and trims keep tool rounds whole

Before this change only terminal line-mode chat replaced old replayed tool
results by a one-line stub (`[tool result cleared: tool <name>, status
<status>, <n> bytes; ...]`) ahead of compaction. Every surface that runs the
engine with a compaction config now does it: the Console, the gateway and rf
sessions, channels, scheduled runs, task and process children, `prx chat --message` and
full-screen chat. At the start of a turn and on each provider round, while the
request is over the warning threshold, the oldest tool results of the request
are stubbed, oldest first, until it fits; the results of the current turn and of
the newest three tool rounds are never stubbed. The effect on those surfaces: a
request that used to go to a handoff compaction or a lossy trim stays whole
more often, and a stubbed result tells the model how to read it again (the
stored document, or a rerun of the tool). Stored rows are unchanged; only the
request differs. `reduction_finished` is emitted with `step =
"clear_tool_results"`.

A trim that cuts the request now removes a tool round (the assistant tool-call
message and its results) as one unit, and a repair pass runs once per provider
round after the reduction steps. A request that used to open on an assistant
tool-call message, or hold a round without its results, which strict providers
(the Gemini family) rejected with `function call turn comes immediately after a
user turn or after a function response turn`, is repaired and logged
(`request repaired for the tool-call protocol`). The gateway replay no longer
replays the tool rounds of the oldest assistant reply when its user message is
outside the 500 rows read.

## The compiled-request event gains a `provenance` object

The `llm.request.context.compiled` row carries a new object key `provenance`:
the system prompt's per-segment manifest (slot, source, stable flag, chars,
bytes, SHA-256), the tool counts and surface hash, history and live-message
counts, the reducers considered for the round, and an empty `hooks` list. It is
additive: no existing field changes, the system prompt text and hash are
byte-identical, hashes and counts only, and nothing is added to the sync
stream. No configuration change is needed. Field by field:
`docs/unified-turn-context.md`.
