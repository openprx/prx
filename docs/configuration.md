# Configuration

OpenPRX uses `~/.openprx/config.toml` as the main configuration file.

For decision admission, backend gates, inspection and rollback, see the
[decision plane reference](decision-plane.md).

The embedded [model catalog](model-catalog.md) owns declared request parameter
defaults and model levels. The existing Codex reasoning override remains an
explicit caller value; the resolver applies the selected model's constraints.
`runtime.codex_reasoning_effort` accepts `none`, `minimal`, `low`, `medium`,
`high`, `xhigh`, or `max`; invalid values fail typed validation when configuration
loads. With no override, known Codex models now use `medium` effort instead of
`xhigh`. Unknown Codex models retain `medium` verbosity and `xhigh` effort.
Kimi Code's temperature constraint follows its selected model, without inferring
model support from the endpoint URL. These changes do not write configuration.

## Quick Setup

```bash
# Interactive setup wizard
prx init --interactive

# Inspect the effective configuration
prx config show
```

Codex CLI `auth.json` import is off unless you set
`[auth] codex_auth_json_auto_import = true`. Sign in with
`prx auth login --provider openai-codex` instead of reusing a Codex client id.
Anthropic setup-tokens are not accepted; use a Console API key. Max, Ultra,
and Team plans include monthly API credits:
<https://support.claude.com/en/articles/17154008>.

## Choosing the Configuration Directory

`init` is the sole persistent setup entry point. The former `onboard` command
has been removed; replace it with `init` in scripts. Bare `prx init` remains
noninteractive and uses the release catalog's default provider/model.

```bash
prx init --spec full --dir /path/to/config
prx init --interactive
prx init --provider openrouter --model openai/gpt-5-mini --from-env PRX_SETUP_KEY
prx init --channels-only

# Grok subscription (SuperGrok / X Premium). Headless hosts use --device-code.
prx auth login --provider xai
prx auth login --provider xai --device-code
```

Full setup over existing configuration requires `--force`. This repairs the
configuration and retains explicit settings; supplied selections replace only
their fields. `--interactive` rejects quick-setup value flags. `--channels-only`
requires existing configuration and accepts only target selection, leaving
models, agents and other sections intact. Interactive setup retains channel,
tunnel, integration, memory, persona and decision setup, plus optional channel
launch after configuration is saved. Noninteractive setup does not launch a
service. Hosted decision credentials still use `--decision-credential-stdin`;
remote decision processing remains separately opt-in.

Configuration generation is journaled. Identity publication and decision
credential/configuration setup have separate completion steps: an error after
the base configuration commits explicitly reports that fact. Do not interpret
every nonzero setup exit as "nothing was written."

### Managed identities

New initialization puts the main role's `SOUL.md`, `AGENTS.md`, `IDENTITY.md`
and `THINKING.md` under `workspace/identities/main/`. Existing root role files
are copied verbatim and retained as migration backups. Once the managed main
identity is published, edit its files; root backup copies are no longer the
active persona. Workspaces not yet migrated continue to load the legacy root
layout. `USER.md`, `TOOLS.md`, `MEMORY.md`, `BOOTSTRAP.md`, `HEARTBEAT.md` and
bootstrap completion state remain workspace-scoped.

```bash
# Preview creation, then apply the same operation.
prx config agents new reviewer --provider openrouter --model openai/gpt-5-mini
prx config agents new reviewer --provider openrouter --model openai/gpt-5-mini --yes
```

Creation automatically writes `identity_dir = "identities/reviewer"` and
creates compact `AGENTS.md` and `SOUL.md` files. If the configuration plan
requires a revision-bound high-risk confirmation, use the revision printed by
the preview. Role IDs are stable lowercase identifiers; `main` is reserved.
Existing roles or directories are never overwritten. A failed transaction
compensates unchanged new files; a crash or indeterminate commit can leave a
`.creation-pending` marker for inspection, and a retry refuses to overwrite it.
Model rebinding with `config agents set` leaves the identity path unchanged.

All three delegation paths now compose identity files followed by nonempty
`system_prompt` instructions. An explicit prompt no longer suppresses the
identity directory in process mode, and task mode honors it as well. Directory
selection does not imply inheritance of the main persona, separate memory or
additional permissions. Process workers copy the selected role into their
temporary worker directory; those copies are not persistent identity sources.

Every command accepts the global `--config-dir <DIR>`. It selects the whole
configuration tree the run reads and writes: `config.toml`, `config.d/`,
`workspace/`, `daemon.lock`, `daemon_state.json`, and the key material under
`keys/`.

An explicit `--config-dir <DIR>` takes precedence over `OPENPRX_WORKSPACE`:
the run's workspace is `<DIR>/workspace`, even when that environment variable
names a different directory. Omit `--config-dir` (and `OPENPRX_CONFIG_DIR`) when
you intend to select the workspace through `OPENPRX_WORKSPACE`.

The session-worker transport key at `keys/session_worker.secret` is published
atomically on first use. Concurrent minting and verification use the same
complete 32-byte key. Existing key material is never overwritten by initialization;
a malformed key is refused and must be restored before using it for worker
capabilities. Existing valid keys remain compatible with older binaries.

Worker transport signing keys are published without replacing a concurrent
writer's key. On Unix, an existing valid key with group or other permissions is
tightened to mode `0600` without rotating it. If shared key publication fails
(for example, on a filesystem without hard links), a warning includes the
filesystem error and identifies the process-local fallback; the key is never
logged. Restarting processes cannot share that fallback key.

`prx init` resolves its target directory in this order, highest first:

1. `prx init --dir <DIR>` — the init-specific target;
2. the global `--config-dir <DIR>`;
3. `OPENPRX_CONFIG_DIR`, then `OPENPRX_WORKSPACE`, then the active-workspace
   marker in `~/.openprx/active_workspace.toml`;
4. `~/.openprx`.

Giving `--dir` and `--config-dir` different directories is an error rather than
a guess: `init` rewrites a whole configuration generation, and picking the wrong
one destroys the structure of a configuration you did not mean to touch. A
resolved target that does not exist yet is created.

`prx init --force` regenerates the configuration it finds in the resolved
directory, preserving the values you set explicitly (see below). When a live
`prx daemon` is running out of that directory, `init --force` prints a notice
that the daemon will hot-reload the regenerated configuration; it is a notice,
not a block.

## Configuration Tree Transactions

`config.toml` and the recognized files under `config.d/` form one effective
configuration generation. PRX stages and validates the complete effective tree
before split, merge, or `prx init --force` mutations, then publishes it through
an odd/even `.config-generation` barrier. Runtime loaders and hot reload retry
while a generation is being committed, so they do not accept a mixed set of
old and new files. A failed commit restores the previous managed files before
the stable generation is republished.

`prx init --force` owns only the recognized managed fragment names. It removes
managed files that are obsolete for the selected preset, but never deletes
unknown operator-owned files under `config.d/`; unknown fragments remain
fail-closed and are not loaded as configuration. A process that finds an odd
generation left by an interrupted commit fails closed; restore the last
known-good configuration and an even generation before restarting.

Raw `prx config plan|apply --file` writes default to `config.toml`. If a later
managed fragment prevents a changed field from taking effect, planning refuses the write
and reports `shadowed by config.d/<file>; use --target <file>` instead of a
misleading no-change plan. Prepare that fragment's complete contents and pass
its basename, for example `--target tools.toml` for `web_search.provider`.
Other files remain unchanged; the tool does not redirect or apply the write
automatically. An unchanged base file is still accepted.

A raw replacement also refuses to omit or empty a stored secret, including a
secret inside a removed table or array. The error names the affected path and
points to `PUT /api/config/secrets`; it never prints the value. This check uses
the file being replaced, including secrets hidden by other fragments or in an
unmanaged file. `config show` is a redacted view, not a file backup: `***` is
never a keep-value marker for a raw replacement. Keep the original stored
values, or use the secrets API to update a credential (`"value": null` clears
it). If the raw-file removal is intentional, pass `--allow-credential-clear` to
both `config plan --file ... --target ...` and `config apply --file ... --target ...`.
The flag permits the omission; it does not fill in missing values, accept
`***` or plaintext in encrypted slots, confirm high-risk changes, or bypass
revision checks. A plan still writes nothing, and apply still needs its normal
confirmation.

Rotating a populated string-array secret (for example, replacing one paired
token with another) does not require the clear flag. Reducing its populated
count does. This compares the number of non-empty entries, not their positions:
`['A', 'B']` to `['', 'C', 'D']` still has two populated entries and is accepted
without clear permission. The rotation and array growth do not count as a drop;
ordinary risk confirmation still applies. If the stored file cannot be read or parsed, replacement is refused
unless clearing is explicitly allowed, because its secret slots cannot be
checked. Invalid posted TOML is refused before the secret-omission check, with
a location-only parse error.

## Runtime Configuration Generations

The disk transaction barrier above is separate from the process runtime
generation. A running daemon has one `ConfigGenerationManager` with:

- `desired`: the latest valid merged configuration accepted from disk;
- `active`: the configuration and runtime objects PRX currently guarantees are
  in effect;
- a monotonic process-local generation id pinned when a turn, message, cron
  job, Xin task, or webhook task is admitted.

The file watcher, config API, and `config_reload` tool all use this same
manager. Components cannot publish configuration directly. A reload is
serialized, validated, and classified before publication:

- snapshot-hot fields apply to newly admitted work while in-flight work keeps
  its pinned generation;
- provider, model, tools, and security-baked runtime objects are rebuilt as one
  candidate and swapped only after preparation succeeds;
- Channels, Cron, Xin/Heartbeat, webhook, and self-system workers use
  generation-scoped supervisors and controlled restart;
- memory/storage/runtime backends, gateway bind/tunnel, module topology, and
  configuration-source paths remain process-restart-only.

For process-restart-only changes, `desired` advances but `active` does not. The
reload response reports those fields in `restart_required`; it never reports
them as live. If candidate construction, readiness, or commit fails, PRX keeps
the old active generation and records the failure in runtime status.

`GET /api/status` exposes the active and desired source revisions, active
generation id, reload state, registered generation participants,
restart-required fields, and the most recent reload failure. Runtime
`message_events` also persist typed `config_generation_id` and
`config_source_revision` columns for SQLite and PostgreSQL.

`evolution_config.toml` is a separate self-system policy document. It is loaded
once when an evolution supervisor generation starts; it has no private file
watcher and cannot publish the process `Config`. The evolution pipeline may
atomically update its in-memory adaptive policy during a run, but a disk policy
change is adopted only through the owning supervisor lifecycle.

Evolution prompt generation and judging use the resolved default model through
the managed provider and physical-attempt accounting path. Configure their
allowance under `[self_system]`:

```toml
[self_system]
evolution_model_max_tokens = 16384
evolution_model_max_micro_usd = 0
evolution_judge_timeout_secs = 180
```

Within one evolution pipeline run, proposal and judge calls share one token,
monetary and time allowance. Passing to an owned proposal or verdict task
does not renew it. Standalone model invocations use a separate allowance.
The token allowance includes input, output and retries; zero disables model
calls. Its maximum is 1,048,576. The monetary allowance defaults to zero, so
paid models need an explicit allowance; local models still need valid price
metadata. Workspace daily/monthly cost limits remain effective. These amounts
are reservation estimates, not vendor billing caps. Missing usage retains an
unknown liability rather than becoming free. The timeout bounds both proposal
and judge calls from the first accounted call in the pipeline. A provider
without physical-attempt accounting is refused.

Prompt generation has no canned-text fallback. A complete model rewrite is
classified at severity 4 and follows the configured human-review threshold,
immutable-section checks, autonomy gate and judge verdict. Default review
settings therefore do not automatically apply a generated rewrite. These
settings do not enable generated executable skill proposals.

## Daily Fitness Snapshots

Fitness v2 scores the most recently closed calendar day from durable runtime
evidence. Reports are operational telemetry and are stored independently at
`<workspace>/self/fitness/daily/YYYY-MM-DD.json`; they are never written to the
memory backend, `MEMORY.md`, or `MEMORY_SNAPSHOT.md`. Evidence includes the
root workspace and process workers below the configured
`sessions_spawn.worker_workspace_root`; descendant queries require an internal
system principal.

```toml
[self_system.fitness]
auto_run = true
timezone = "Asia/Tbilisi" # IANA timezone defining the calendar day
run_at = "00:10"          # local HH:MM; evaluates the previous day
retention_days = 180
max_backfill_days = 7
min_samples = 5
min_coverage = 0.60
```

The five weights remain task quality 35%, no-repeat behavior 25%, proactive
execution 20%, learning effectiveness 10%, and efficiency 10%. A metric with
missing or too-small evidence is marked `unavailable` or `insufficient_data`
and contributes no synthetic fallback value. The available weights are
renormalized only when `min_coverage` is met; otherwise `final_score` is null.
Efficiency compares token and latency medians with the preceding 30 days, so a
local model with zero API price is not automatically assigned a perfect score.

Use `prx fitness status`, `prx fitness run`, and `prx fitness history`. Before
removing reports created by older releases, inspect
`prx fitness migrate-legacy`; add `--apply` to create an online SQLite backup,
archive the old JSON, delete its exact key namespace transactionally, rebuild
the snapshot, and verify that both database and Markdown projections are clean.
The old `self_system.fitness_interval_hours` key remains accepted only for
configuration compatibility and no longer controls scheduling.

## Example Configuration

```toml
# ~/.openprx/config.toml

[general]
default_provider = "anthropic"
default_model = "claude-opus-4-6"
temperature = 0.3
max_history = 200

# [gateway] has no request timeout. An HTTP request that starts an agent turn
# is allowed to take as long as the turn takes; the gateway runs such work as a
# detached job instead of holding it inside the request future, so a disconnect
# never destroys it. Use `prx tasks list` / `prx tasks kill <id>` (or
# `GET /api/jobs`) to see and end long work.

[gateway]
# Trust boundary. The gateway binds to loopback by default and refuses a public
# bind unless a tunnel is configured or `allow_public_bind = true`.
# `trust_forwarded_headers` (default false) tells it that a reverse proxy sits in
# front: it then takes the client IP from `X-Forwarded-For` / `X-Real-IP` for
# rate limiting, and the caller identity of MCP tool calls from
# `X-Client-Cert-Subject`, `X-Forwarded-Client-Cert`, `X-Spiffe-Id`,
# `X-Spiffe-Svid` and `X-Client-Cert-Pem`. With it off those identity headers
# are ignored (the first request carrying each header logs a warning that names
# the header, never its value; later ones log at debug) and an MCP caller must
# present a bearer JWT. Enable it only when every request passes through a proxy that
# removes or overwrites all of these headers; otherwise any client can claim any
# identity, including a SPIFFE SVID copied from another peer.
# trust_forwarded_headers = false
#
# How long an already-accepted platform webhook delivery (`/whatsapp`, `/linq`,
# `/nextcloud-talk`) is remembered, in seconds.
#
# These three routes answer the platform as soon as the delivery is accepted and
# run the agent turn as a detached job, because every sender on them enforces a
# response deadline no agent turn can promise to meet (Linq documents a 10-second
# response timeout, and Meta retries a WhatsApp delivery "with decreasing
# frequency ... for up to 7 days"). Fast acks make redelivery rare but not
# impossible — a dropped connection or a proxy timeout still produces one — so
# each delivery is recorded and a redelivery of the same payload is answered
# instead of run a second time.
#
# Size it against the sender's retry schedule, not against the agent: a window
# shorter than the platform's retry window lets a late retry start the turn
# again. Must be greater than 0. Default: 86400 (24 hours).
# channel_webhook_dedup_ttl_secs = 86400

[runtime]
# Extra host environment variable names available to the generic shell tool.
# Default: []; exact uppercase ASCII names only, no wildcards or values.
# Adding a name grants access to its value and is a high-risk config change.
# Takes effect on process restart. See tools.md, "Shell environment".
# shell_env_allowlist = ["PRX_BUILD_CACHE"]

# Hard cap on the tokio blocking-thread pool (optional).
#
# PRX places no ceiling on concurrent turns, sub-agents, or sessions, which
# makes this pool the last implicit concurrency gate in the process — and it
# fails by deadlock rather than rejection: once every slot is held, further
# blocking work (SQLite writes, subprocess reaps, config reloads) queues
# forever with no timeout.
#
# Leave it unset to derive the cap from `available_parallelism()`, which is
# cgroup-aware, with a floor of 2048 and a ceiling of 16384. Set it explicitly
# to go beyond that ceiling, or to pin a value for reproducible capacity
# planning. Must be greater than 0.
#
# Read during process bootstrap, before the tokio runtime exists, so it takes
# effect only on restart: hot reload cannot resize a live pool.
# max_blocking_threads = 8192

# Seconds after which a still-running work item is reported in the log.
#
# NOTIFICATION ONLY — nothing is ever terminated because of this threshold.
# PRX does not cut work off on a clock: a research run, a build, or a long
# agent turn can legitimately take hours, and ending one on a timer is a worse
# outcome than letting it finish. The warning exists so a task that is
# genuinely wedged becomes visible, leaving the decision to an operator
# (`prx tasks list`, then `prx tasks kill <id>` if it really is stuck).
#
# Unset uses the 900s default. Set 0 to silence the warning entirely. Enabled
# values must be at least 10 seconds; below that the sweeper would warn about
# ordinary tool calls and bury the signal it exists to raise.
# long_task_warn_secs = 900

# Seconds an agent turn may produce NO OBSERVABLE PROGRESS AT ALL before it is
# judged hung and terminated.
#
# This is NOT a turn timeout. The window is reset by every sign of life — a
# provider stream chunk, a tool call starting or finishing, history compaction,
# output written to a channel — so a turn that keeps working runs as long as it
# needs to, however long that is. Only complete silence for the whole window
# counts, and complete silence is a stall, not a slow task.
#
# Compare `long_task_warn_secs` directly above; they are opposites and must not
# be conflated:
#
#   long_task_warn_secs  measures elapsed run time, is NOT reset by progress,
#                        and NEVER terminates anything (warning only).
#   idle_hang_secs       measures time since the last progress event, IS reset
#                        by progress, and DOES terminate the turn.
#
# A healthy six-hour turn is warned about repeatedly and never terminated. The
# defaults are deliberately different numbers (900 vs 1800) so an operator
# always sees the warning well before anything is killed.
#
# Completed serving-request preparation counts as progress (`request_prepared`).
# The event follows actual compilation; merely starting an iteration does not
# report a provider response or repeatedly refresh a pending request's window.
#
# Termination goes through the normal cancellation path and the runtime
# registry's kill path, so child process groups are cleaned up (`killpg`) and
# the event is recorded in the registry. The error says explicitly that the turn
# hung, which is how it is told apart from a task that failed and from a turn an
# operator ended with `prx tasks kill`.
#
# Unset uses the 1800s default. Set 0 to disable hang detection entirely.
# Enabled values must be at least 30 seconds; below that an ordinary quiet tool
# call could be mistaken for a hang.
# idle_hang_secs = 1800

[memory]
backend = "sqlite"
# Compatibility gate for semantic promotion. Message events are controlled below.
auto_save = true

[memory.events]
record_user_messages = true
record_assistant_messages = true
record_tool_events = false

[memory.semantic]
auto_promote_user_messages = true
auto_promote_assistant_messages = false
min_chars = 30

# Optional standalone external-event receiver. Durable topic, participant,
# memory, ingestion-state, and outbox writes use one SQLite transaction.
[webhook]
bind = "127.0.0.1:16899"
token = "replace-with-a-secret-token"
# When set, requests must also send X-Webhook-Signature: sha256=<HMAC hex>.
signing_secret = "replace-with-a-separate-hmac-secret"

[channels_config.signal]
account = "+1234567890"
dm_policy = "allowlist"
allowed_from = ["uuid:your-uuid"]

[channels_config.wacli]
webhook_listen = "127.0.0.1:16868"
webhook_path = "/wacli"
webhook_secret = "replace-with-secret"
store_dir = "/path/to/wacli-store" # required for group-title fallback and inbound downloaded images
# Optional Message Yourself owner-command mode. Requires bot_jid or bot_number
# and a wacli build with `send text --allow-self-send`.
# bot_jid = "1234567890@s.whatsapp.net"
# self_chat_mode = true
# Outbound [IMAGE:…]/[DOCUMENT:…] markers may name any readable path by default.
# Set to true to confine them to the workspace again (see docs/channels.md).
outbound_media_workspace_only = false

# Local ramflux agent-host bridge. prx holds no ramflux keys.
# authorization_mode: follow_rf (default) | supervised | full
# full is effective only when bridge.hello also declares authorization = "full".
[channels_config.ramflux]
authorization_mode = "follow_rf"
max_in_flight_per_conversation = 1
on_new_message = "queue" # queue | interrupt
turn_wall_clock_secs = 120
encrypt_at_rest = true
# socket_path = "/path/to/ramflux.sock"

[channels_config.ramflux.accounts.agent_a]
# system_prompt_file = "personas/agent_a.md"
# model = "example-model"
# provider = "example-provider"
# scope_profile = "default"
# cross_conversation_retrieval = false

[heartbeat]
interval_minutes = 30
active_hours = [8, 23]

[agent]
# Agent tool rounds have no numeric ceiling. A progressing turn continues until
# it completes, fails explicitly, or is cancelled with Esc / `prx tasks kill`.
# Read-only tool calls in one model iteration always run in parallel; there is
# no switch that turns that off. Tool execution is never time-bounded either —
# a long-running tool is normal agent business. Use `prx tasks list|kill` to see
# and stop work that is actually running away.
# Stateful tools that mutate the same workspace, runtime configuration, memory
# or session lifecycle share a serialization barrier. Waiting for that barrier
# is bounded to 60 seconds; if it cannot be acquired, that call returns an error
# without running its backend. This contention bound does not limit a tool once
# its execution has started.
# Max concurrent read-only tools in one batch (default: 2)
read_only_tool_concurrency_window = 2
# Enable priority scheduling so foreground tools run before background batches.
priority_scheduling_enabled = false
# Optional list of low-priority/background tools.
# NOTE: priority is matched by tool name only (not by action), so `cron` is not
# listed here — adding it would demote every cron action, not just background runs.
low_priority_tools = ["sessions_spawn", "delegate"]

# Per-turn capability policy. `always_include` prefers direct publication;
# `always_exclude` removes names before decision admission. `channel_exclude` applies to IM
# channel sessions only (Signal / Telegram / Discord / WhatsApp / …) and defaults
# to the operations/development surface: wasm_plugins_manage, wasm_plugin_reload,
# wasm_plugins_status, hooks_manage, hooks_status, proxy_config, config_reload,
# gateway, mcp_status, nodes, git_operations, skills_manage, memory_reindex,
# document_sync, document_ingest. Terminal chat, the gateway and the console are
# unaffected. Name a tool in `always_include` to put it back on channels, or set
# `channel_exclude = []` to drop the default entirely. `always_include` does not
# override `always_exclude`: a name in `always_exclude` is excluded everywhere.
[tool_tiering]
always_include = []
always_exclude = []
# channel_exclude = []

# Skill catalog published in the system prompt. A catalog at or below `top_k` is
# published whole, in a canonical order, which keeps that section of the prompt a
# pure function of the installed skills instead of a retrieval result that moves
# with the wording. The section carries metadata only (instruction bodies stay
# behind `skill_read`), which is why the default is large. Larger catalogs fall
# back to relevance retrieval.
[skill_rag]
top_k = 32

# WASM plugins. Off by default: nothing under <workspace>/plugins is scanned
# or loaded, and no plugin tool, provider, memory backend, hook, cron job or
# middleware exists. Read at process start. Which plugins may run, and with
# which permissions, is decided by the operator trust file `plugin-trust.json`
# beside this file, never by anything inside the workspace (see "WASM plugins").
[plugins]
enabled = false

# See docs/skill-lifecycle.md for concepts, commands and recovery.
# Skill lifecycle: new instruction skills and revisions are evaluated against an
# operator-owned frozen suite, gated and only then published. Everything here is
# operator-owned; no tool or model output can change it. The section is read at
# process start (changing it needs a restart) and is validated when the
# configuration loads. Off by default: nothing is offered to the agent and no
# store is opened.
#   mode = "draft_only"  drafts and reviews only (default)
#   mode = "shadow"      adds bounded evaluation and gating, never publishes
#   mode = "auto"        publishes an eligible proposal after every gate
# A weaker publication policy is refused at load, not downgraded:
# `require_holdout = false` and `min_cases_floor` below 3 do not load.
# `allow_executable_proposals = true` enables explicit private script drafts
# from operator templates, never automatic publication. Executable bundles
# still enter through a separate import of the exact operator-reviewed hash.
# `storage_dir` only accepts the default, the location
# `skill_read` and recovery use. `owner_scope` names the owner whose store this
# installation manages and is required when enabled; an agent run from another
# owner, or from a caller whose owner the runtime cannot derive, does not reach
# the lifecycle actions of `skills_manage` (the refusal names both owner ids,
# and `prx skills lifecycle status` shows the configured one). The runtime
# derives the caller's owner id from its trusted scope: the scope's `owner_id`
# when it has one (gateway, console and webhook turns carry it), otherwise
# "owner:<workspace>:<channel>:<sender>" composed from the workspace path, the
# channel the session runs on (terminal chat, the Web Console and each IM
# channel have their own channel name, for example "telegram") and the sender
# (the operator or chat user id). Set `owner_scope` to exactly that id for the
# sessions that should manage skills. An owner-isolated caller without its own
# owner id never matches. Each `suites`
# entry registers the frozen suite (by SHA-256 digest) that governs one skill.
# Operate it with `prx skills lifecycle status|list|inspect|publish|cancel|
# rollback|force-close`. `force-close` (CLI only) closes an authorized proposal
# stuck on a transaction conflict as failed or stale with a recorded reason; it
# never publishes and leaves the active revision as it is (a publication that
# turns out to be live is recorded Committed instead of closed). Automatic
# proposal scheduling is not wired; enabling it does not start proposal runs.
# With the daemon, an enabled lifecycle runs a recovery sweep at start
# (retrying with backoff while
# another process holds the store lock) and `status` lists any proposal it had
# to skip. Publishing or rolling back through `skills_manage` needs a signed
# one-shot approval grant bound to the proposal (or to skill, revision and
# observed generation for a rollback), in every autonomy level; the grant is
# spent on use. `skills_manage` action `lifecycle_import_executable` publishes a
# reviewed directory in auto mode, with a signed one-shot grant bound to `source`
# and `revision_id` (the complete bundle hash). Enabling the lifecycle does not
# authorize executable imports or waive execution-time review lineage checks.
[skills_lifecycle]
enabled = false
mode = "draft_only"
# owner_scope = "<trusted runtime owner id>"
storage_dir = "self/skill-lifecycle"
require_holdout = true
min_cases_floor = 3
max_concurrent_evaluations = 1
max_proposals_per_cycle = 1
max_edit_tokens = 300
max_edit_fraction = 0.20
allow_executable_proposals = false
# [[skills_lifecycle.suites]]
# skill_id = "release-notes"
# suite_digest = "<64 lowercase hex digits>"

# Optional exact-model tool allowlists constrain decision-eligible tools.
# Models without an entry keep the ordinary tool set. An empty list deliberately
# exposes no tools. Quote model names containing `/` or `:` in TOML.
[tool_tiering.model_allowlists]
"gemma4-e2b" = ["shell", "file_read", "file_write"]

# Secure autonomous defaults. `full` skips confirmation prompts but remains
# workspace-scoped and bounded unless the operator explicitly widens it.
[autonomy]
level = "full"
workspace_only = true
forbidden_paths = ["/etc", "/root", "/home", "/opt", "/tmp", "~/.ssh"]
max_cost_per_day_cents = 500

# Multi-agent setup
[agents.researcher]
provider = "anthropic"
model = "claude-sonnet-4-6"
agentic = true
allowed_tools = ["web_search", "file_read"]

# Model fallbacks
[reliability.model_fallbacks]
claude-opus-4-6 = ["claude-sonnet-4-6"]

# Provider fallbacks
fallback_providers = ["xai"]
```

When the `wasm-plugins` feature is enabled, an installed provider component can
be selected with `default_provider = "wasm:<exported-name>"`. Likewise,
`memory.backend = "wasm:<exported-name>"` selects an installed storage
component. The explicit `wasm:` namespace is required so plugins cannot shadow
built-in provider or memory backend names. These selections are supported by
TUI chat, both agent CLIs, gateway turns, standalone IM channels, delegated
agents, and process-isolated session workers.

An unrestricted profile is always explicit: set `workspace_only = false`,
clear `forbidden_paths`, and widen both ceilings deliberately. `prx doctor`
warns when all four unrestricted choices are active together.

### Decision admission and autonomy

The shared turn decision service derives runtime effect permission from its
pinned execution security policy: `readonly` excludes side-effecting tools
(`read_only` is accepted as a compatibility alias; saved values use `readonly`)
and delegation, `supervised` requires applicable approval, and `full` permits
those candidates to be considered. This does not guarantee selection or bypass
scope ACLs, model support, cost limits, structural restrictions, or inherited
child capability limits. Rehearsal remains read-only in all three modes.

Under `supervised`, a capability without exact call-scoped operation IDs cannot
supply an approval grant. It can be deferred or excluded and the turn can close;
this is not a promise that every write or delegation opens an approval prompt.
A resumed approval remains bound to its original exact call and cannot authorize
a different capability. Configuration defaults are unchanged.

### Scope rules

`[[autonomy.scopes.rules]]` narrows what a given sender / channel / chat type
may do. A rule matches when its `user`, `channel`, and `chat_type` criteria all
match; an unset criterion matches anything, and `"*"` matches explicitly.

A rule carries two independent ACLs, and they are **evaluated differently**:

| Field | Governs | Deny/allow order | Rule ordering |
|---|---|---|---|
| `tools_allow` / `tools_deny` | which tools may run | `tools_deny` first, then `tools_allow` (empty = no whitelist) | first matching rule decides |
| `send_allow` / `send_deny` | which recipients `message_send` may reach | `send_deny` first, then `send_allow` (empty = no whitelist) | every matching rule that carries an outbound entry is consulted |
| `assign_allow` / `assign_deny` | which `prx chat` sessions may be handed work | `assign_deny` first, then `assign_allow` (empty = **no reach at all**) | every matching rule that carries an assignment entry is consulted |

The outbound half is not first-match-wins on purpose. A rule with neither
`send_allow` nor `send_deny` says nothing about recipients and is skipped
entirely, and `send_deny` is checked against **every** matching rule. Without
that, a rule written for some unrelated reason — a `tools_deny`-only rule over
the same scope, say — would match first and silently disable every `send_deny`
below it, with no error and no denial. Only `send_allow` still depends on
order: the first matching rule with a non-empty whitelist is the one that
applies, and a whitelist that an earlier rule already shadows is a hard config
error rather than dead configuration.

When no rule matches, or a matching rule sets no `send_allow`, the destination
channel decides: sending on the turn's **own** channel is allowed (the
long-standing behaviour), and `message_send`'s explicit `channel` argument
pointing anywhere else is denied. Cross-channel sending is therefore opt-in per
sender, and a denial is never retried on the current channel.

Outbound entries are `{channel}:{recipient}`; a bare `"*"` matches every
destination and either segment may be `"*"`. The entry is split at its first
colon, so recipients that contain colons themselves (JIDs, group ids) stay
intact. A malformed entry — no colon, or an empty segment — is a hard config
error, because a `send_deny` typo that silently matched nothing would fail
open.

#### Recipient spellings

The same person is reachable under more than one string. `send_deny` therefore
matches the recipient after folding the spellings that are **derivable from the
identifier itself**:

- a JID's `:device` suffix, in either position (`1234@s.whatsapp.net`,
  `1234:2@s.whatsapp.net` and `1234@lid:2`), and the server's letter case;
- one leading `+` — the WhatsApp Cloud API strips it before sending, so
  `+15550001111` and `15550001111` are one account;
- a UUID's letter case and hyphenation, both of which Signal accepts.

A colon in a recipient that is *not* a JID is left alone, because it is part of
the identifier: a Telegram forum topic is `{chat_id}:{thread_id}` and denying it
must not deny the parent chat.

`send_allow` gets **none** of this folding and is matched exactly as written.
Folding is a guess about which two spellings mean the same person; a wrong guess
may refuse a send, but it must never be what grants reach to one.

> **Aliases that are not derivable are not covered.** A WhatsApp LID
> (`<id>@lid`) carries a different number from the same contact's phone JID
> (`<number>@s.whatsapp.net`), and a Signal account's UUID is unrelated to its
> E.164 number. Correlating either pair needs a mapping table — whatsmeow's
> `whatsmeow_lid_map`, or the Signal server — that the policy layer does not
> hold, and that may simply be absent, in which case "no mapping" would read as
> "not the same person" and fail open. Write one `send_deny` entry per
> identifier form the contact can appear under, or deny the whole channel with
> `"{channel}:*"`, which is the only alias-proof form.

```toml
[[autonomy.scopes.rules]]
channel = "wacli"
# WhatsApp turns may never message this number, on any channel.
send_deny = ["*:+15550001111"]

[[autonomy.scopes.rules]]
user = "uuid:untrusted-user-uuid"
tools_allow = ["memory_recall"]        # whitelist-only tools
send_allow = ["telegram:12345"]        # whitelist-only recipients
```

The outbound decision defaults to **allow for the channel the turn already
owns** and **deny for any other channel**: sending a reply back where the
message came from behaves exactly as it did before scope rules existed, while
reaching a different channel requires an explicit `send_allow` entry. Rejections
name only the destination channel and the recipient's stable audit fingerprint,
never the plaintext recipient. `autonomy.scopes.default` applies to tool access
only — it does not gate recipients.

A send that arrives over `POST /api/channels/{name}/send` (what `prx chat` uses,
see [`[chat.daemon]`](#chatdaemon)) has no inbound conversation behind it, so it
is authorized as a send **from the operator plane channel `api`** by an unknown
sender with an unknown chat type. Two consequences: it is always cross-channel,
so it is denied until an operator opts in; and rules for it are written against
`channel`, since `user` and `chat_type` cannot match it.

```toml
[[autonomy.scopes.rules]]
channel = "api"
# Let a chat session message this one WhatsApp number through the daemon.
send_allow = ["wacli:15550001111@s.whatsapp.net"]
```

A turn driven by the gateway's webhook surface is the same story from the other
side. It owns no messaging channel of its own — its replies leave through
whichever channel handle the daemon registered — so scope rules are matched
against the channel the turn actually **came from**: `webhook` for
`POST /webhook`, and the platform's own name (`whatsapp`, `linq`, …) for a
channel webhook. The same-channel default still measures against the handle the
turn replies through, so naming the origin only brings more rules into play; it
never mutes a turn that no rule constrains.

```toml
[[autonomy.scopes.rules]]
channel = "webhook"
# A generic webhook turn may answer, but never reach out to anyone else.
send_deny = ["telegram:*", "signal:*"]
```

### Assigning work to a `prx chat` session

A message arriving on a channel can make a **specific** `prx chat` session do
something: the daemon keeps a mailbox per registered session and chat pulls from
it (`POST /api/chat-sessions/{id}/assign`, see the chat-session API). Because an
assignment starts work on a *different* entry point, it is governed separately
from outbound messaging and the default is the other way round.

**Assignment is default-deny.** With nothing configured, no channel
correspondent can assign anything — not even on their own channel, and
regardless of `autonomy.scopes.default`, which governs tool access only. There
are exactly three ways through:

1. **An owner.** `autonomy.scopes.assign_owners` lists `{channel}:{sender}`
   origins that may hand work to any session this daemon knows about. The
   identity compared is the one the runtime recorded for the turn, not anything
   a model or a request body claimed.
2. **An explicit grant.** `assign_allow` on a matching rule names the sessions
   that scope may reach, as `{label}:{session_id}` pairs.
3. **The operator plane.** A request that satisfied the gateway's bearer token
   is already the local operator and does not consult these rules. It may
   optionally name an origin (`on_behalf_of`), which only *adds* that origin's
   rules — it can never widen what the token already permits.

`assign_deny` sweeps **every** matching rule, so it can never be shadowed by an
earlier one, and it outranks both `assign_allow` and `assign_owners` — an owner
named in a deny entry is refused. Only `assign_allow` depends on order, and a
whitelist an earlier rule already shadows is a hard config error, exactly as for
`send_allow`.

Assignment entries are matched **literally**: a chat session id is generated by
this daemon and has one spelling, so there is no alias to fold and no guess to
make. The `label` half is the name the chat process registered under, which is
convenient for following one workstation across restarts — but it is
**self-declared**, so where that matters, name the session id instead.

```toml
[autonomy.scopes]
# The phone that owns this daemon may assign work to any chat session.
assign_owners = ["wacli:15550001111@s.whatsapp.net"]

[[autonomy.scopes.rules]]
channel = "telegram"
# A Telegram correspondent may only drive the session labelled "workstation",
# and never the one quarantined below.
assign_allow = ["workstation:*"]

[[autonomy.scopes.rules]]
assign_deny = ["*:11111111-2222-3333-4444-555555555555"]
```

Refusals name the origin and the target session only by their stable audit
fingerprints, never in plaintext — the same fingerprints the work registry and
the session listing use, so one party reads identically everywhere.

Every accepted assignment gets a row in the work registry (`prx tasks list`) for
as long as it is outstanding, and killing that row discards it from the mailbox
and records a `cancelled` result. That is deliberate: nothing here expires, so a
session that has stopped pulling is *reported* as `silent` and never evicted,
and being seen is what makes a stuck assignment endable.

**The mailbox survives a daemon restart.** Registrations, queued assignments,
acknowledgements and results are written through to
`{workspace}/state/chat-mailbox.db` (SQLite, created `0600`) before the caller
is answered, and the gateway replays the file on the way up. Three consequences
are worth knowing:

- **A chat keeps its session id and its token.** Only the token's SHA-256 is
  stored, so the check that authenticates a pull after a restart is the same
  check it was before one. A chat does not need restarting, and does not
  re-register.
- **Work handed out but never acknowledged goes back to the head of the queue**,
  exactly as it does on the next pull. Work the session *did* acknowledge stays
  acknowledged: the chat holding it usually outlives the daemon, and
  redelivering it would be the daemon deciding a live chat had lost it.
- **Liveness is not reset by the restart.** A restored session reports the age
  of its last real pull, so a chat that died with the daemon reads as `silent`
  rather than as freshly `polling`.

Durability never costs availability: if the database cannot be written, the
mutation is logged at `error` and the in-memory mailbox carries on, so a full
disk degrades the guarantee instead of refusing live work. Deleting the file
while the daemon is down is therefore a supported way to reset the mailbox — a
chat whose session went with it registers again by itself on its next poll.
Degraded durability is reported rather than only logged: the mailbox publishes
`writes_total`, `writes_failed`, `lock_contention` and `durable` next to every
other database this process keeps, in `prx tasks pools` and
`GET /api/runtime/pools`.

Three further guarantees hold while more than one request touches a session at
once:

- **A later transition always wins on disk.** Mailbox writes leave the session
  state lock before they run, so a contended database cannot freeze a session;
  each write carries the revision it was planned at and the store refuses one
  older than the row already holds. An acknowledgement therefore cannot be
  overwritten by a delivery that was planned before it, and no restart hands
  acknowledged work back out.
- **Retiring work and recording its result are one commit**, as are a
  deregistration and the cancellations that cascade with it. A write that fails
  leaves the work outstanding — where the next pull or the next restart finds
  it — instead of losing the work and the answer together.
- **Reporting a result is idempotent.** A chat whose report lost its response
  retries it and is answered with the sequence the first report recorded, for as
  long as that record is retained. A retry never mints a second completion.

Mailbox persistence runs on the blocking pool rather than on the API's async
workers, and one request spends **one** contention budget however many rows it
touches. A database another process is holding therefore slows the chat sessions
using it and leaves the rest of the API answering.

### `[chat.daemon]`

`prx chat` deliberately opens no IM connection: a second listener on the same
account would race the daemon for inbound messages. Its `message_send` therefore
asks the daemon to deliver, which is an ordinary authenticated call to the
daemon's control plane.

| Key | Default | Meaning |
|---|---|---|
| `chat.daemon.url` | derived from `[gateway] host`/`port` | Base URL of the daemon gateway. The default is right whenever chat and daemon share a host; `0.0.0.0` resolves to loopback. |
| `chat.daemon.token` | empty | Bearer token for the control plane. Required unless the daemon runs with `gateway.require_pairing = false`; without it the daemon answers `401` and the send is refused. |

```toml
[chat.daemon]
url = "http://127.0.0.1:16830"
token = "zc_..."
```

The token is the same kind `prx tasks --token` takes. A paired token is stored
in `gateway.paired_tokens` as a hash, not as plaintext, so the value here is the
one handed out once at pairing time — a hash copied from the config file will
not authenticate.

#### Being assigned work

The same two keys decide whether this chat *takes* work as well as sends it.
When either `chat.daemon.url` or a usable credential is present, chat registers
itself with the daemon at startup, polls its mailbox, runs what it is handed
through the ordinary turn machinery, and reports the reply back. With neither
set it does not try: the fallback daemon address is `[gateway]`'s own bind,
which every chat has whether or not a daemon listens there, so an unconfigured
chat that tried anyway would announce a refusal on every start.

- **It never blocks startup.** Registration runs in the background; a daemon
  that is down costs this chat the ability to be assigned work and nothing else,
  and says so once in the transcript.
- **It registers under the workspace directory's name.** That is the `{label}`
  segment an `assign_allow` entry matches. The daemon treats it as self-declared
  and does not verify it, so name the session id instead when that matters.
- **Assigned work is never silent.** Each arrival is announced with the channel
  and redacted sender it came from and how it was dispatched, and each result
  handed back is announced too.
- **The poller is ordinary in-flight work.** `/sessions` lists it and
  `/kill --daemon d<N>` ends it, the same as any other daemon request.
- **The mailbox is not rate limited.** `gateway.api_rate_limit_per_minute`
  bounds what a caller may spend on the API; the two mailbox endpoints
  (`POST /api/chat-sessions/{id}/inbox/pull` and `.../result`) are exempt from
  it, because they are one local process polling for work already addressed to
  it rather than a caller reaching in. What guards a mailbox is the per-session
  token, not a quota. Every other route keeps its quota, `assign` included, so
  raising the number of open chats no longer costs the operator theirs.
- **Exiting withdraws the session.** Nothing on the daemon side expires a chat
  session, so a chat killed with `SIGKILL` leaves a row listed as `silent` until
  an operator reaps it with `DELETE /api/chat-sessions/{id}`. The row survives a
  daemon restart too — the mailbox is persisted — so reaping it is an operator
  act rather than something a restart does by accident.
- **A daemon restart no longer loses assignments.** Queued work is replayed from
  `{workspace}/state/chat-mailbox.db` and the chat carries on pulling with the
  same session id and token. If the file *is* removed, the pull is refused with
  `unknown_session` and the chat registers again by itself on its next poll.
- **A report of work that was killed is refused, not acknowledged.** Reporting an
  assignment twice answers with the original sequence, so a retry after a lost
  response is safe. Reporting one an operator killed in the meantime is answered
  `409` with code `assignment_cancelled` and the reason the work ended: the
  cancellation already stands in the result feed, and the late report is neither
  recorded as a second result nor silently acknowledged as the session's own.

The three dispositions map onto paths chat already had: `queue` joins the input
backlog, `steer` joins it in front (the same place `/now` puts a typed line),
and `interrupt` ends the turn in flight through the same local path `Ctrl+C`
uses and *then* joins it in front. `interrupt` is an explicit act by whoever
assigned the work — never a timeout, and nothing here runs a clock.

Agentic delegates fail closed when `allowed_tools` is missing or empty. Named
entries select only matching eligible parent tools. Use `allowed_tools = ["*"]`
to explicitly inherit every eligible parent tool except `delegate`; the
wildcard cannot be mixed with names. Tool inheritance does not bypass the
child runtime envelope, scope policy, side-effect gate, approval, or audit.

Compliance controls are operator-classified and evidence-bearing. Generated
server/full configurations enable the first-contact AI interaction notice but
leave the EU risk classification as `unclassified`; PRX does not infer legal
applicability. See [Evidence-bearing compliance controls](compliance-controls.md)
for high-risk classification, declaration, incident workflow, evidence, and
rollback examples.

`HEARTBEAT.md` remains the editable periodic checklist. When heartbeat is
enabled, its dash-bullet entries are reconciled into stable recurring Xin
tasks; Xin is the always-available execution scheduler.
In that heartbeat-only mode, ordinary Xin tasks and goals stay disabled.
Reordering the file preserves task IDs, removed entries are disabled, and the
configured prompt, interval, and active-hour window remain authoritative.

Xin has an operator CLI matching the LLM tool management surface:

```bash
prx xin status
prx xin list
prx xin add health-watch "Inspect service health" --recurring --interval-secs 3600
prx xin update <task-id> --priority high
prx xin run <task-id>
prx xin runs <task-id> --limit 20
prx xin pause <task-id>
prx xin resume <task-id>
prx xin cancel <task-id>

prx xin goals add release-checklist
prx xin steps add <goal-id> verify "Verify release artifacts"
prx xin goals get <goal-id>
prx xin goals cancel <goal-id>
```

Use `pause` to prevent future claims while allowing active work to finish. Use
`cancel` to revoke an active task/step lease and stop that execution. Calendar,
timezone, and delayed scheduling remain the responsibility of `prx cron`; Xin
owns autonomous interval work and durable ordered workflows.

## Shared Memory Fabric

PRX treats `chat`, `agent`, `gateway`, `channel`, `delegate`, and `sessions_spawn` as different message entrypoints over one workspace memory fabric.

- `message_events` stores normalized user, assistant, tool, and worker events.
- `memory_events` is the outbox/cursor stream used by SQLite polling watchers.
- `memories` stores promoted long-term semantic facts.
- `auto_save` no longer controls the base message log; it gates semantic promotion for backward compatibility.
- The embedded `Agent` API additionally honors `semantic.auto_promote_user_messages` and `semantic.min_chars`. It promotes only original user input, using an owner-scoped `user_msg:<run-id>` key and source session/event metadata. It leaves legacy `user_msg` entries intact. Unsupported owner-aware writes are skipped with a content-free warning; model execution continues. See [embedded API admission](unified-turn-context.md#embedded-agent-api-admission).

`[memory.events]` controls raw/quasi-raw event recording. Turning it off stops new fabric event rows for entrypoints wired through `MemoryFabric`.

`[memory.semantic]` controls promotion from event/message content into durable semantic memory. Assistant promotion is disabled by default to reduce noisy self-generated memory.

For `sessions_spawn`, `task` runs an in-process sub-agent and `process` launches a worker process. Process mode uses a manifest with `memory_strategy`, `shared_memory_db_path`, and `worker_memory_db_path`; the default `shared_fabric` strategy writes worker events into the parent workspace fabric while still keeping the execution boundary explicit. Set `[sessions_spawn].process_memory_strategy = "isolated_private"` for a private worker DB. `hybrid` is fail-closed because no production merge consumer or merge/reject/ack/cleanup protocol exists; use `shared_fabric` or `isolated_private` instead.

Recursive child delegation requires both an operator setting and an explicit
`allow_delegation: true` spawn request:

```toml
[sessions_spawn]
allow_recursive_delegation = false
max_recursive_depth = 1
```

The permission defaults to false, including when loading an older config.
When permission is enabled, `max_recursive_depth` must equal 1. Values above 1 are refused while
permission is enabled; the current strict tool-subset contract supports one
additional level. Each child remains a proper
subset of its authenticated parent, cannot replenish depth or budget, and keeps
the selected provider/model pins through descendants. Recursive publication
subtracts a strict half-share of the remaining monetary balance, rounded down
to whole cents; the parent retains the rest. Approval resume keeps the original
run's consumed child slot and balance; if that state is unavailable after a
restart, further recursive grants are refused. A request without the
opt-in keeps the existing exact model-only scope. See [recursive child grants](tools.md)
for admission, audit/status and rollback behavior.

The two modes also differ in the credential a named agent's child uses when the agent's `provider` is the gateway's own provider (any spelling of the same identity). Task mode reuses the gateway's provider chain, with the root `api_key` at the root `api_url`, and ignores the agent's own `api_key`. Process mode passes the agent's `api_key` to the worker, which presents it to that provider's default catalog host, because an `[agents.<name>]` entry has no URL field. In both modes a key only goes to its own provider identity.

## Standalone Webhook Ingestion

`[webhook]` is the authenticated external-event receiver used to synchronize
topics. `token` is always required when enabled. `signing_secret` is optional;
when configured, bearer/token authentication and a valid HMAC-SHA256
`X-Webhook-Signature` are both required.

Durable ingestion currently supports configured `sqlite` and `lucid` memory
backends. Other configured backends fail startup explicitly instead of silently
writing a separate local `brain.db`. SQLite/Lucid ingestion persists a durable
pending/committed/failed state and atomically commits the topic, participant,
eligible memory, and memory-fabric outbox row. Failed or expired pending attempts
can be retried with the same idempotency identity.

## Removed Configuration Keys

prx does not cap how much work runs at once and does not impose timeouts on
agent work, so the keys that expressed those ceilings are gone. A few others
named limits no code path ever applied.

A config that still contains one of these keys **keeps loading**: the key is
dropped and a `WARN` names the file and line so you can delete it when
convenient. Nothing rewrites your config files. A `POST /api/config` body
or a `config.patch` that carries one of these keys is applied without it,
and the response's `warning` names the key; a raw
`PUT /api/config/files/{name}` stores the file as written (the key has no
effect) and its response carries the same `warning`.

| Removed key | Why |
|---|---|
| `agent.max_tool_iterations` | progressing agent work has no arbitrary tool-round ceiling |
| `agent.parallel_tools` | no concurrency ceiling to switch |
| `agent.tool_dispatcher` | never read: the tool-call wire form follows the provider's capabilities |
| `agent.read_only_tool_timeout_secs` | no timeouts on agent work |
| `agent.concurrency_kill_switch_force_serial` | staged rollout retired |
| `agent.concurrency_rollout_stage` | staged rollout retired |
| `agent.concurrency_rollout_sample_percent` | staged rollout retired |
| `agent.concurrency_rollout_channels` | staged rollout retired |
| `agent.concurrency_auto_rollback_enabled` | staged rollout retired |
| `agent.concurrency_rollback_timeout_rate_threshold` | staged rollout retired |
| `agent.concurrency_rollback_cancel_rate_threshold` | staged rollout retired |
| `agent.concurrency_rollback_error_rate_threshold` | staged rollout retired |
| `sessions_spawn.max_concurrent` | sub-agent fan-out is uncapped; use `prx tasks` to see and end runs |
| `sessions_spawn.max_spawn_depth` | legacy global limit is ignored; explicit recursive grants use `max_recursive_depth` |
| `sessions_spawn.max_children_per_agent` | per-session fan-out is uncapped |
| `scheduler.max_concurrent` | due cron jobs all start at once; use `prx tasks` to see and end them |
| `xin.max_concurrent` | due Xin work is no longer capped by an arbitrary configuration ceiling |
| `xin.max_tasks` | the persisted Xin task store no longer has an arbitrary task-count ceiling |
| `autonomy.max_actions_per_hour` | no hourly action budget |
| `gateway.request_timeout_secs` | no request deadline on gateway handlers |
| `channels_config.message_timeout_secs` | no wall clock on a channel or chat turn; a stalled turn is ended by `runtime.idle_hang_secs`, a running one by `prx tasks kill` |
| `runtime.idle_hang_max_total_secs` | no absolute wall-clock ceiling on a progressing turn |
| `agents.<name>.max_iterations` | delegated agents have no arbitrary tool-round ceiling |
| `security.audit.sign_events` | audit signing was never implemented; the setting had no effect |
| `[security.resources]` (whole table) | never enforced by any code path |
| `memory.events.retention_days` | never read by the hygiene pass |
| `causal_tree.observability.decision_log`, `causal_tree.observability.metrics` (and the earlier `write_decision_log`, `write_metrics`) | never read; decision events follow the `[observability]` backend |

The `sessions_spawn` tool also no longer accepts `max_iterations` or
`timeout_seconds`; task-mode and process-mode sub-agents share the same
unbounded execution contract.

A key that is merely **misspelled** is still a hard error. Only the exact paths
above are absorbed, so `max_actions_per_hourr` still stops the load with
`Unknown configuration path(s)`.

## Workspace Files

| File | Purpose | Editable by agent |
|------|---------|-------------------|
| `SOUL.md` | Core values and personality | Never |
| `AGENTS.md` | Operating rules | Yes |
| `THINKING.md` | Cognitive framework | High bar |
| `IDENTITY.md` | Self-description | Yes |
| `MEMORY.md` | Long-term memory | Yes |
| `HEARTBEAT.md` | Periodic task checklist | Yes |
| `USER.md` | User profiles and permissions | Observations only |
| `TOOLS.md` | Tool-specific notes | Yes |
| `memory/YYYY-MM-DD.md` | Daily logs | Auto-created |

These files are automatically injected into the agent context at startup.

## Memory Backends

| Backend | Description |
|---------|-------------|
| **SQLite** | Default, local, FTS5 full-text search + vector search |
| **Lucid** | Lightweight markdown-based memory |
| **PostgreSQL** | Scalable, multi-user |
| **Markdown** | File-based, human-readable |

An enabled causal decision plane for `prx agent` requires a memory backend
that persists `message_events`: `sqlite`, `lucid`, or `postgres`. This storage
requirement does not establish selected-retrieval support: the candidate's
bounded Memory/Document/Session ports are implemented for SQLite and Lucid's
local SQLite store; PostgreSQL ports still fail explicitly. See the
[decision-plane backend support matrix](decision-plane.md#candidate-memory-backend-support).
Startup
refuses `memory.backend = "none"` or `"markdown"` while the plane is enabled,
because neither backend can record the inbound event or causal telemetry.
The plane also requires `memory.events.record_user_messages = true` so the
inbound event can be linked to each decision. Choose a supported backend and
enable user message recording, or disable `[causal_tree].enabled` before
starting the agent. A disabled plane keeps the existing memory behavior.

Decision-linked Agent terminal markers include a versioned authentication
binding to the original inbound event, owner, workspace, decision chain,
terminal claim, and assistant projection. Recovery verifies these records
before reconstructing an outcome. It confirms the stored outcome after the
writer drains; queue admission alone is not confirmation. A completed fixed
reply can repair its missing directive-applied event using the original
source's configuration generation and revision.

Recovery requires the original workspace decision key and all referenced
records. An unsigned marker, an older claim, a missing retained record, a
changed row, or a replaced key causes recovery to fail explicitly. Keep the
original key and database backup together when restoring a workspace. A
rollback must preserve these records; do not remove authentication fields or
rewrite old markers to make them replayable. Recovery repairs telemetry and
does not authorize another provider or tool execution.

### Embeddings and vector recall

New installations enable credential-free local embeddings by default:

```toml
[memory]
embedding_provider = "local"
embedding_model = "prx-local-hash-v1"
embedding_dimensions = 384
vector_weight = 0.7
keyword_weight = 0.3
```

The built-in provider is deterministic, keeps memory content on the host, and
requires no model download or API key. It combines token and character n-gram
features, so it improves similarity recall over keyword-only search without
claiming the semantic quality of a dedicated embedding model. Set
`embedding_provider = "none"` to opt out explicitly, or configure `openai` or
`custom:<URL>` with a compatible model for richer semantic matching. Changing
provider, model, or dimensions requires reindexing existing vector-bearing
memory rows; stale vectors are not mixed with the active index.

#### Upgrading from a configuration written before 0.8.10x

`memory.embedding_provider` defaulted to `"none"` before 0.8.10x. A
configuration that never wrote the key therefore turns local vector recall **on**
when the binary is upgraded, with no configuration change: recall then spends
one embedding computation and one similarity scan per turn. Nothing else about
the memory store changes, and keyword/FTS recall is unaffected.

`prx doctor memory` reports the inherited default, and `prx chat`, `prx daemon`,
`prx gateway` and `prx channel start` log it once at startup:

```
embedding_provider not set; defaulting to local vector recall since 0.8.10x,
set memory.embedding_provider = "none" to disable
```

`prx chat` redirects its log to `chat.log`, so for that entry point the line is
also written to stderr before the TUI takes the screen and shown once on the
first screen of the session:

```
note > embedding_provider not set; defaulting to local vector recall since 0.8.10x, set memory.embedding_provider = "none" to disable
```

Write the key either way to settle it and silence the notice:

```toml
[memory]
embedding_provider = "none"   # keyword/FTS recall only
```

`prx init --force` regenerates managed templates while preserving explicit
settings. Recognized legacy settings first pass through the same migration used
by the normal configuration loader. The previous tree is copied to
`config.d.pre-<version>-<timestamp>/` before regeneration. Templates supply
structure, comments and defaults; incompatible causal settings are refused.

The local candidate reads migration inputs from one consistent configuration
generation and checks its revision again when committing. A concurrent change
refuses the stale plan instead of overwriting it. Unknown and legacy-named
fragments remain excluded by the existing loader and are preserved unchanged.
This implementation has not yet passed the centralized migration/rollback tests.

The report is printed by the command itself — `prx init` runs before any log
subscriber exists, so it does not go through the log at all. The archive
directory and every preserved key are listed on stdout:

```
PRX configuration initialized (full)
  Config dir: /home/you/.openprx
  Previous configuration archived to /home/you/.openprx/config.d.pre-0.8.126-20260914101500123
  Preserved 4 explicit setting(s): memory.embedding_provider, ...
  Added 2 new default(s) this version ships: memory.vector_candidate_cap, ...
  Reset 1 unset setting(s) to refreshed defaults: agent.max_iterations
```

##### What `--force` does to each key

`--force` sorts every key in the managed templates into exactly one of three
classes, and prints each class as its own line:

| Class | What it means | What lands on disk |
|---|---|---|
| **preserved** | you wrote the value | your value |
| **added-default** | this version ships a key your tree never had | the new default |
| **reset** | you never changed the key and its shipped default has moved | the new default |

Telling "you wrote it" apart from "an older `prx init` wrote it" needs a record
of what the templates themselves produced, so `prx init` writes one next to
`config.toml`:

```
~/.openprx/.init-defaults.toml     # mode 0600, not part of the merged config
```

Without the sidecar, ordinary template-default refresh treats existing values
as explicit. The decision-plane migration has its own versioned contract:
Schema 1 or current schema 2 with `enabled = false` remains explicitly disabled, even
when the sidecar also records false. Recognized unversioned legacy-default
signatures, including the old bare `enabled = false` template, are treated as
unset by the existing loader migration. An unversioned false alongside modern
non-legacy settings remains explicit. The staged candidate now defaults the
local plane to true and writes the version marker in the full init template;
adoption, behavioral migration and rollback acceptance remain pending.

For other settings, a value equal to the previous shipped default is refreshed
with that default because the sidecar cannot distinguish an explicit identical
value from an untouched template.

##### Decision-plane default and migration

The staged source default and full `prx init` template change together:

```toml
[causal_tree]
schema_version = 2
enabled = true
```

Minimal/server templates inherit the same central default. Remote decision
processing retains `remote_enabled = false`; `auto` plus an empty endpoint does
not authorize a physical request. The 2026-10-05 source addendum requires
source-owned token and monetary reservations at pinned PMM prices before each
supported physical attempt. Missing ownership, pricing, accounting support or
remaining budget refuses admission. This is an estimate, not a vendor billing
cap; the default change grants no provider or credential permission.

| Existing configuration | Candidate behavior |
| --- | --- |
| No causal section, or no `enabled` value | Inherits true |
| Unversioned bare `enabled = false` from the old template | Recognized legacy default becomes unset, then inherits true |
| Unversioned false with legacy-only keys | Legacy classification removes the default false and migrates compatible settings |
| Unversioned false alongside modern non-legacy settings | Explicit false remains false |
| Schema 1 or current schema 2 and false, including development snapshots | Explicit false remains false on load, save, JSON/management and `init --force`, even if its sidecar also records false |
| Explicit true | Remains true; backend and cost gates still apply |
| Incompatible legacy value or conflicting old/new keys | Migration refuses; `init --force` must not install an enabled template as fallback |

The same loader migration handles the merged main file and supported managed
fragments. Ordinary load normalizes in memory; saving or authorized regeneration
writes canonical keys. `init --force` uses its existing consistent snapshot and
revision-checked publication, and preserves recognized explicit disables before
generic default refresh. The current schema marker is 2; it is not a rollout date
or an expiry for development configurations. Existing policy-table serialization
and explicit policy values are unchanged.

To disable deliberately, retain the current schema version and set `enabled = false`
through the existing management flow. Causal changes remain restart-required;
the desired file revision and active runtime generation are separate. Preserve
prior revisions, decision/source/terminal evidence, approval state and cost
journals during rollback. Disabling cannot refund or cancel already dispatched
work. This prepared source cutover neither changes a real configuration nor
restarts a service. See [Decision plane](decision-plane.md#enable-disable-and-rollback).

##### Quoted keys and archives

Quoted keys and quoted table headers are ordinary configuration —
`[cost.prices."gpt-4.1"]`, `[reliability.model_fallbacks]` with `"gpt-4.1" = [...]`
— and are folded and re-emitted with their quoting intact. Anything `--force`
still cannot place is named on stderr and makes the command exit non-zero; it is
never dropped quietly. The same applies to a key this build can no longer
represent — one whose parent the templates now write as a scalar — which is
listed under its own heading and exits non-zero as well:

```
WARNING: could not fold 1 setting(s) into the regenerated templates, ...
WARNING: reset 1 setting(s) this build can no longer represent: routing.mode.legacy
```

Each `--force` run gets its own archive. The stamp carries milliseconds and a
taken name gets a `-2`, `-3` suffix, so two runs in the same second produce two
archives rather than one overwriting the other. Archives are never cleaned up
and they contain the encrypted credentials that were in the tree at the time, so
prune `config.d.pre-*` yourself after a key rotation.

An archive holds every file `--force` is about to rewrite: `config.toml`, each
managed `config.d/*.toml` fragment, any unmanaged `config.d/*.toml` fragment of
your own, and the `.init-defaults.toml` defaults record. A tree written by a
build older than 0.8.132 has no defaults record and is archived without one,
which is not an error. Restore all of them together and the next `--force`
classifies against the baseline the restored tree was written with; restore
`config.toml` alone and the record left on disk describes a different version,
so delete `.init-defaults.toml` in that case and every value in the restored
tree is treated as yours.

In the local candidate, malformed, unreadable or unstable configuration trees
are refused before installation; `--force` no longer replaces them with bare
templates. A tree containing causal settings also refuses migration,
preservation or staged-validation failures. An archive already created before
refusal is retained. Correct the reported configuration problem before retrying.

A successfully read tree with no causal settings retains the existing generic
validation-repair fallback: it can install bare templates, print the archive
and validation warning, and exit non-zero. Neither that fallback nor a failed
revision check silently overrides a protected decision-plane setting.

#### Vector recall candidate cap

Cosine similarity has no index, so vector recall scores a bounded window of the
newest embedded memories rather than the whole table:

```toml
[memory]
# Embedded memories one vector recall may score. Default: 2000.
vector_candidate_cap = 2000
```

This is a genuine trade-off, not a formality. In a store holding **more**
embedded memories than the cap, an old-but-highly-relevant memory is never
scored and therefore never recalled — no matter how well it matches. Raise the
cap to buy recall with per-turn latency; lower it to buy latency with recall.
Vector recall emits a warning the first time a recall actually hits the cap, so
a store that has outgrown its setting says so:

```
Vector recall hit its candidate cap: older embedded memories are no longer
scored and cannot be recalled. Raise [memory] vector_candidate_cap to score
more of the store.
```

The window is ordered by `update_seq`, an integer counter the store bumps on
every insert and every update, not by `updated_at` and not by `rowid`.

`updated_at` is a local-offset RFC 3339 string ordered as text, so a DST
transition, a machine timezone change, or a store shared between processes in
different zones reorders whole blocks of rows and truncates the wrong ones.
`rowid` has no timezone but does not move either: writing an existing key
rewrites its content and embedding in place, so a key refreshed a second ago
kept the position it was created in and, in a store larger than the cap, stayed
outside the window. "Newest" therefore means most recently **written** —
created or refreshed — and re-saving an old memory brings it back into recall
immediately.

Existing stores migrate on open: the column is added, every row already present
takes its insertion order as its starting sequence, and the candidate index is
rebuilt to cover the new ordering key. No configuration change is needed, and
the cap above is unrelated to this ordering — raising it widens the window,
it does not decide which rows the window holds.

### SQLite Connection Pool

The SQLite backend is the default. It does **not** use the PostgreSQL pool
shape, because SQLite allows only one writer per database: N peer connections
would not raise write throughput, they would only collide and return
`SQLITE_BUSY`. Instead every write shares one serialized writer connection,
while reads — which WAL journalling makes genuinely concurrent — are served from
a pool:

```toml
[memory]
backend = "sqlite"
# Reader connections for the SQLite backend.
# Unset = 2 per CPU, clamped to 4..32. Must be greater than 0 when set.
sqlite_read_pool_size = 16
# Max seconds to wait when opening the database file. Unset = wait forever.
sqlite_open_timeout_secs = 30
```

Like the PostgreSQL pool, this is a connection *reuse* mechanism rather than a
concurrency limiter: when every reader is checked out, callers **queue** for one
instead of failing, and that wait has no deadline. Writes queue for the writer
the same way.

This key sizes the memory database only. The WhatsApp session store has its own
(`channels_config.whatsapp.read_pool_size`) so the two databases can be tuned
separately, even though both run the same pool implementation.

Two settings are database-level rather than user-facing:

- **WAL journalling** is enabled at startup and is a prerequisite, not a tuning
  knob — without it readers and the writer would lock each other out and a
  reader pool would be pointless.
- **No `busy_timeout`.** If the database file is locked by another process,
  SQLite retries until the lock clears rather than returning `SQLITE_BUSY`, and
  logs a warning roughly every two seconds while it waits. A `busy_timeout` is
  a timeout by another name: after it expires the caller gets an error for a
  lock that is merely held by someone else.

Reader connections are opened with `PRAGMA query_only`, so a statement that
tries to mutate the database on a reader fails immediately instead of silently
contending with the writer.

### PostgreSQL Connection Pool

The PostgreSQL backend reuses connections through a pool configured under
`[storage.provider.config]`:

```toml
[storage.provider.config]
provider = "postgres"
dbURL = "postgres://user:password@localhost:5432/openprx"
schema = "public"
table = "memories"
# Maximum live connections (default: 32). Must be greater than 0.
pool_max_size = 32
# Timeout in seconds for establishing a connection (default: 15).
connect_timeout_secs = 15
```

The pool is a connection *reuse* mechanism, not a concurrency limiter. Size
`pool_max_size` against the database's own `max_connections` budget rather than
against expected task concurrency: when every connection is checked out, callers
**queue** for one instead of failing, and that wait has no deadline.

`connect_timeout_secs` is the only timeout the backend applies. It bounds the
TCP/startup handshake, which is a network fault rather than a long-running task.
No `statement_timeout` is set, so queries are never cut off — long-running
queries are normal for agent workloads.

Connections are health-checked on checkout: one closed by the server (restart,
failover, idle reaper) is discarded and transparently replaced.

Three components build a pool from these settings when the provider resolves to
`postgres`: the memory backend, the cron store, and the durable webhook
ingestion repository. They are separate pools, so the process can hold up to
`3 * pool_max_size` connections. Size the value against the database's
`max_connections` accordingly — the default of 32 needs a `max_connections` of
at least ~100 to leave room for other clients.

### Running the PostgreSQL memory tests

The behavioural PostgreSQL tests (memory backend, cron store and scheduler,
durable webhook repository, schema-migration inspection and the fitness CLI
migration) need a live, disposable database, so they are marked `#[ignore]`. A plain `cargo test` (including CI,
which has no PostgreSQL service) counts them as **ignored**, never as passed.
To run them, point `OPENPRX_TEST_POSTGRES_URL` at a database where the test
role may create and drop schemas, with the `vector` extension available, and
ask for the ignored tests explicitly:

```bash
OPENPRX_TEST_POSTGRES_URL=postgres://user:password@localhost:5432/prx_test \
  cargo test --all-features --lib -- --ignored --test-threads=1 postgres
```

`--test-threads=1` is needed because the cron tests share the cron tables across
the database. Narrow the filter (for example `memory::postgres`) to run one
area.

The row-level-security tests only prove something under a role that is neither
`SUPERUSER` nor `BYPASSRLS`. Running with `--ignored` but without the variable
fails every such test instead of skipping it. The statement-text guards
(including `tests/pgvector_binding_regression.rs`) need no database and run in
every `cargo test`.

## WhatsApp Web Session Store

The Web-mode WhatsApp channel (selected by `session_path` when
`phone_number_id` is absent) keeps its Signal-protocol
state — identities, sessions, pre-keys, sender keys, app-state MACs — in the
SQLite database at `session_path`. Every inbound and outbound message performs
several lookups against it, so under concurrent chats this store, not the
network, is the busiest thing in the channel.

It is pooled the same way as the SQLite memory backend, and for the same reason:
WAL journalling makes reads genuinely concurrent while SQLite still permits only
one writer, so reads come from a pool and writes share one serialized writer
connection.

```toml
[channels_config.whatsapp]
session_path = "~/.openprx/state/whatsapp-web/session.db"
# Concurrent read connections (default: 2 per CPU, clamped to 4..32).
# Must be greater than 0 when set.
read_pool_size = 16
```

Acquiring a connection has **no deadline**: when every reader is checked out, or
the writer is busy, callers queue instead of failing. Queueing is counted and
timed rather than rejected, so a saturated store shows up as latency in the
pool counters rather than as errors in the message path.

If the database file is locked by another process, SQLite retries until the lock
clears rather than returning `SQLITE_BUSY`, logging a warning roughly every two
seconds while it waits. Reader connections are opened with `PRAGMA query_only`,
so a statement that tries to mutate the database on a reader fails immediately
instead of silently contending with the writer.

This key sizes the session database only; the memory backend has its own
(`memory.sqlite_read_pool_size`).

## Security

For source identity, execution binding, decision approvals and interruption
boundaries, see [Runtime security](security.md).

- **Sandboxing**: Bubblewrap, Firejail, Landlock (Linux kernel), Docker
- **DM/Group policies**: Allowlist / open / disabled per channel
- **Context compaction**: Token-threshold trigger with full-chain propagation
- **Path validation**: Workspace-scoped file access with symlink protection
- **Memory ACL**: Per-user, per-project access control
- **Encrypted secret store**: For API keys, OAuth tokens

### Secrets in configuration

One registry (`src/config/secret_paths.rs`) decides which configuration paths
hold secrets, and which name an endpoint. Every configuration read surface
uses it: `GET /api/config`, `GET /api/config/files`, `prx config show`, the
`Debug` form of the configuration, change plans and `428`/`4xx` bodies,
history records, `config.patch` output, `GET /api/mcp/servers` (server URL,
last error and configuration error), and the agent `proxy_config` and
`nodes` tools. Log lines and errors that name an endpoint (OTEL, Signal,
node errors, a blocked workspace MCP server, MCP discovery errors, the
"cannot reach a running PRX process" error of the daemon message tool and
`prx tasks`) show it without userinfo. A transport error from a library
elsewhere can still quote a URL, so keep credentials out of URLs (see
below).

- `prx config show` and `GET /api/config` print `***` for every registered
  path, and for any key whose name looks like a credential (`*_token`,
  `*_secret`, `*_key`, `password`, …), so a secret in a section added later is
  masked even before it is registered. Both surfaces mask the same paths.
- Fields that name a provider or an endpoint by URL (`default_provider`,
  `api_url`, `reliability.fallback_providers[]`, `model_routes[].provider`,
  `embedding_routes[].provider`, `agents.<name>.provider`,
  `providers.tool_dialects[].provider`, `router.models[].provider`,
  `memory.embedding_provider`, `smart_group.classifier_provider`, the
  keys of `[cost].prices`, and the endpoint URLs `proxy.http_proxy`,
  `proxy.https_proxy`, `proxy.all_proxy`, `mcp.servers.<name>.url`,
  `chat.daemon.url`, `nodes.nodes[].endpoint`,
  `observability.otel_endpoint`, `media.audio_ollama_url`,
  `tunnel.custom.health_url`, `mcp_server.jwt_jwks_uri`,
  `a2a.card_jwks_uri` (published in the public Agent Card, so userinfo
  there is refused at load), `channels_config.mattermost.url`,
  `channels_config.nextcloud_talk.base_url`,
  `channels_config.matrix.homeserver`, `channels_config.signal.http_url`,
  `channels_config.whatsapp.ws_url`) are shown with their URL userinfo, every
  query value (whatever the parameter is called), every valueless query
  parameter and the fragment replaced: `custom:http://alice:pw@host/v1?sig=x#t`
  reads `custom:http://[redacted]@host/v1?sig=[redacted]#[redacted]`. A
  parameter name that does not read like a name (`sk-live-x`, a long token)
  is redacted with its value, and a configured URL is redacted to its end
  (a value holding `,` or `)` is not cut there). Plans, history records, `428`
  bodies, `GET /api/config`, `GET /api/mcp/servers`, validation refusals,
  `prx config status`, `prx models default show`, `prx providers test`,
  `prx doctor`, the `prx` listings and log lines use this one redactor;
  posting a redacted value back unchanged keeps the stored one, and a
  `[redacted]` part anywhere else is refused. When two stored `cost.prices`
  keys redact alike, a posted redacted key is ambiguous and is refused. The agent `proxy_config` and `nodes`
  tools show proxy and node URLs the same way. Setting or changing such a
  URL with userinfo is a credential change (high risk, needs confirmation).
  Userinfo is redacted, not encrypted: it stays as written on disk even
  with `secrets.encrypt = true`, because several of these fields double as
  provider identities that must match as stored. Move a credential out of a
  URL into its registered field (`api_key`, a channel token,
  `nodes.nodes[].bearer_token`) where one exists. A file holding such a URL is served
  masked by the raw file API. Posting a redacted label back unchanged keeps
  the stored one, credential included; a redacted label anywhere else (a new
  endpoint, a changed `fallback_providers` array) is refused with `400`
  naming the path, and nothing is written. Prefer `api_key` over URL userinfo
  all the same.
- The raw file API (`GET /api/config/files`, `PUT /api/config/files/{name}`,
  the Web Console's advanced raw editor) treats secrets as write-only. A file
  that holds a secret is returned as a masked rendering with
  `"redacted": true, "editable": false` (comments are not preserved in that
  rendering); a file without secrets is returned byte for byte. A comment
  that assigns a credential-named key (`# api_key = "sk-old"`) counts as a
  secret, so such a file is also returned masked, without its comments. The
  key may be separated by `=` or `:` and spelled in any case with `-` for
  `_` (`# API-Key: sk-old`), and a name ending in `apikey` counts
  (`# apikey = …`). A credential written in free text inside a comment
  (`# the old key was sk-…`) is not detected and is served as written:
  delete such comments rather than rely on the masking. A
  `PUT` is refused with `400` when it carries the mask `***` at a secret path, or, with
  `secrets.encrypt = true`, plaintext at an encrypted slot; the refusal names
  the path, never the value. A file that carries its `enc2:` values exactly as
  stored on disk is accepted; the API never returns ciphertext, so that only
  applies to a copy taken from the file itself. A TOML syntax error is reported by line and column only, and
  validation errors never quote a secret the request carried.
  Omitting or emptying a stored secret is also refused, naming its path and
  `PUT /api/config/secrets`. For an intentional raw replacement, this `PUT`
  accepts `?allow_secret_clear=true`, equivalent to the CLI's
  `--allow-credential-clear`. This permission does not replace high-risk
  confirmation or `If-Match`; prefer the secrets API to clear an individual slot.
  In practice this makes the common files read-only in the raw editor: on
  a paired instance `network.toml` holds `gateway.paired_tokens`, and once a
  root `api_key` is stored `config.toml` holds it, so both are served
  masked with `"editable": false`. Only a file that holds no secret (for
  example `cost.toml` or `observability.toml` in the usual layout) can be
  edited as text. Change the others through the structured editor or
  `POST /api/config`, and credentials through `PUT /api/config/secrets`.
- `POST /api/config` never stores the mask. A masked object member keeps the
  stored value; an array posted back exactly as `GET /api/config` returned it
  (for example `model_routes` with `"api_key": "***"`) keeps its stored
  secrets; a changed array that still carries `***` is refused with `400`
  naming the path, and nothing is written. No error message quotes a
  secret the request carried.
- A credential that an older release's `GET`→`POST` round trip stored as the
  literal `***` is reported at every load (`Stored configuration holds the
  redaction mask`, with the paths, never a value) and is refused by
  `POST /api/config` until it is repaired: the error names the stored slots.
  Set a provider slot with `PUT /api/config/secrets`, or send the affected
  array (for example `gateway.paired_tokens`) with real values.
- `PUT /api/config/secrets` is the write-only entry point for a credential:

  ```sh
  curl -X PUT http://127.0.0.1:16830/api/config/secrets \
    -H "Authorization: Bearer $PRX_TOKEN" -H 'Content-Type: application/json' \
    -d '{"slot": "model_routes.fast.api_key", "value": "sk-..."}'
  # => {"slot":"model_routes.fast.api_key","status":"stored","activation":"<reload status>",...}
  ```

  Slots: `api_key`, `agents.<name>.api_key`, `model_routes.<hint>.api_key`,
  `embedding_routes.<hint>.api_key`, `reliability.api_keys` (a list),
  `composio.api_key`, `browser.computer_use.api_key`,
  `web_search.brave_api_key`, `storage.provider.config.db_url`, and every
  other registered path by its concrete path, for example
  `channels_config.telegram.bot_token`, `mcp.servers.<name>.env.<VAR>` (a
  new variable is added), `nodes.nodes[0].bearer_token`, `webhook.token`, or
  `gateway.paired_tokens` (a list). `"value": null` clears the slot; a
  credential its section requires (a channel's bot token) cannot be cleared,
  and a channel, MCP server or node that is not configured is refused. The credential goes through the structured save (encrypted
  when `secrets.encrypt = true`) and is activated; no response or log line
  contains it. An unknown slot, an agent, route, channel, MCP server or node
  that is not configured, a hint shared by two routes, `***`, ciphertext, or
  a value with control characters is refused with `400`; read-only autonomy
  answers `403`.

  Rotating an outbound credential (a key or token PRX presents to someone
  else: provider keys, bot tokens, passwords, `nodes.nodes[].bearer_token`,
  `chat.daemon.token`) at the named slot is confirmed by the request itself.
  Two kinds of slot are not such credentials and answer `428` like
  `POST /api/config` until confirmed:
  - an MCP server environment variable (`process_environment_changed`;
    besides a token it can be `NODE_OPTIONS`, `LD_PRELOAD`, `DYLD_*`,
    `PYTHONPATH` or `PATH`, which change what the server runs);
  - a secret whose value admits a client (`access_grant_changed`, D-048):
    `gateway.paired_tokens`, `webhook.token`, `webhook.signing_secret`,
    `channels_config.webhook.secret`, `channels_config.whatsapp.verify_token`,
    `channels_config.whatsapp.app_secret`,
    `channels_config.lark.verification_token`,
    `channels_config.lark.encrypt_key`, `channels_config.linq.signing_secret`,
    `channels_config.nextcloud_talk.webhook_secret`,
    `channels_config.wacli.webhook_secret`, `nodes.server.bearer_token` and
    `nodes.server.hmac_secret`. Whoever holds a new value is let in, and the
    holders of the old one are shut out (for the restart-only paired tokens,
    at the next restart).

  Retry with
  `?confirm_high_risk=true` and `If-Match: "<revision>"` from the `428`
  body; a change in between answers `409`. The confirmation is bound to
  that revision: `?confirm_high_risk=true` without `If-Match`, or with
  `If-Match: *`, answers `428` again (`required: "if_match"`). `If-Match`
  is accepted on every call; another revision than the configuration the
  slot is written into answers `409`. `storage.provider.config.db_url` is a
  connection string, so a new value also re-points memory storage
  (`endpoint_changed`) and needs the confirmation too.

  Limits of the slot syntax: a value is trimmed and must not be empty, hold
  a line break, or start with `enc:`/`enc2:`; an MCP server or variable
  name containing `.` cannot be addressed. Every MCP `env` value is treated
  as a secret, so the raw editor refuses a plaintext one even for a setting
  such as `LOG_LEVEL`; set such values (empty, multi-line, or dotted names)
  with `POST /api/config`, which encrypts them too.

Every registered path is encrypted (`enc2:`) when `secrets.encrypt = true`
and masked on every read surface. A guard test walks the configuration
schema and fails when a field whose name looks like a credential is neither
registered nor exempted with a reason (today only `nodes.server.tls_key`,
which is a file path), so the table stays complete.

| Group | Registered paths |
| --- | --- |
| Provider | `api_key`, `agents.<name>.api_key`, `model_routes[].api_key`, `embedding_routes[].api_key`, `reliability.api_keys[]` |
| Tool and storage | `composio.api_key`, `browser.computer_use.api_key`, `web_search.brave_api_key`, `storage.provider.config.db_url` (and its accepted aliases `dbURL`, `database_url`, `databaseUrl`) |
| Gateway and MCP | `gateway.paired_tokens[]`, `mcp.servers.<name>.env.<var>` (every value, whatever the variable is called) |
| Channels | `channels_config.telegram.bot_token`, `discord.bot_token`, `slack.bot_token`, `slack.app_token`, `mattermost.bot_token`, `matrix.access_token`, `dingtalk.client_secret`, `email.password`, `irc.server_password`, `irc.nickserv_password`, `irc.sasl_password`, `lark.app_secret`, `lark.encrypt_key`, `lark.verification_token`, `linq.api_token`, `linq.signing_secret`, `nextcloud_talk.app_token`, `nextcloud_talk.webhook_secret`, `qq.app_secret`, `wacli.webhook_secret`, `webhook.secret`, `whatsapp.access_token`, `whatsapp.app_secret`, `whatsapp.verify_token` (all under `channels_config.`) |
| Webhook, tunnel, daemon | `webhook.token`, `webhook.signing_secret`, `tunnel.cloudflare.token`, `tunnel.ngrok.auth_token`, `chat.daemon.token` |
| Remote nodes | `nodes.nodes[].bearer_token`, `nodes.nodes[].hmac_secret`, `nodes.server.bearer_token`, `nodes.server.hmac_secret` (and the accepted alias `token` of both `bearer_token` fields) |

Not recognized as secrets: a credential inside MCP server `args`, inside
`tunnel.custom.start_command`, or in a URL's query string (`?token=`); only
URL userinfo is redacted. Keep credentials in the registered fields.

`prx-node --config` pointed at the PRX configuration file decrypts only the
`nodes.server` credentials, with the `.secret_key` beside that file; it never
creates a key. A copy of the configuration on another host needs that key
file too, or plaintext values (write them with encryption off); a missing
key or a value that does not decrypt stops `prx-node` with an error naming
the key file or the setting, never a value.

A plaintext value in an encrypted slot, as an older release wrote it, is
still read as it is and is encrypted on the next save; nothing is rewritten
at startup. With encryption on, the raw file API refuses plaintext at every
registered path; set such a value through `PUT /api/config/secrets` or
`POST /api/config`, which encrypt it.
**Downgrade:** once a newer release has saved the configuration, an older
release reads every newly encrypted value (route and rotation keys, channel
and bot tokens and passwords, paired tokens, MCP environment values, webhook,
tunnel, daemon and node credentials) as the literal `enc2:` string: a
channel fails to authenticate, an MCP server receives the ciphertext as its
environment, and paired clients are no longer accepted. Before downgrading,
turn encryption off with the newer release, which saves every slot as
plaintext:
`POST /api/config?confirm_high_risk=true` with `{"secrets": {"encrypt": false}}` (or edit
`[secrets] encrypt = false` and save once with the newer release). While
encryption is on, no PRX surface writes these keys as plaintext:
`PUT /api/config/secrets` encrypts, and the raw file API refuses plaintext.
A rollback to a history record taken while encryption was on restores
`enc2:` values: if one happens after encryption was turned off, turn it off
again before downgrading.

### MCP server launch environment

A stdio MCP server is a program PRX starts as its own user. It starts from a
cleared environment: it inherits only `PATH`, `HOME`, `LANG`, the `LC_*`
locale variables and `TMPDIR` of PRX's environment (on Windows also
`SYSTEMROOT`, `USERPROFILE`, `TEMP` and `TMP`), plus the server's own
`mcp.servers.<name>.env`. Nothing else of PRX's environment reaches it, in
particular no provider key exported in the shell PRX was started from. A
server that needs another variable must name it in its `env` (an `env`
change is high risk, see below).

A stdio child no longer inherits, for example, `HTTPS_PROXY`, `NO_PROXY`,
`SSL_CERT_FILE`, `USER` or `XDG_RUNTIME_DIR`: a server behind a proxy or a
custom CA, or one that uses a keyring, needs them listed in its `env`.

A stdio server starts in a dedicated empty directory, `mcp/<server>-<hash>`
beside `config.toml` (created owner-only; the hash of the exact name keeps
`a b` and `a/b` apart), never in the directory PRX was started from, where a
`.npmrc`, a `node_modules/.bin` or a shadowing Python module could change
what runs. A symlink or a file at that place (or at `mcp/`) is refused, never
followed or `chmod`ed. Without a configuration directory, a stdio server
without `working_dir` is not started (it is never started in the workspace).
`mcp.servers.<name>.working_dir` sets another: it must be absolute (checked
at load), and a change that sets it must name an existing directory that is
not a symlink (checked when the change is planned). Changing it is a launch
change (high risk). `prx doctor` warns when it lies inside the workspace.

When a configuration change stops running a server (a narrowed
`mcp.workspace_servers`, a removed or disabled server), the running process
of that server is stopped when the new configuration is activated, and the
replaced configuration never starts it again; servers both configurations
run keep their process. An HTTP server's host
is also checked when the connection is made: a name that resolves to a
private or local address then is refused, not only at load (through an HTTP
proxy, the proxy resolves the name instead).

A workspace `mcp.json` is not an operator surface: the workspace file tools
can write it. It is limited to narrowing what `config.toml` allows:

- The servers `config.toml` defines keep running whether or not `mcp.json`
  lists them. For such a server the whole definition comes from
  `config.toml` (`transport`, `command`, `args`, `env`, `url`, timeouts,
  `tool_name_prefix`, `working_dir`). The workspace may disable it
  (`"disabled": true`) or expose fewer of its tools (`allow_tools` is
  intersected with the operator's, `deny_tools` is added); anything else it
  sets for such a server is ignored with a warning naming the fields. An
  allow-list that leaves no tool disables the server.
- A server `config.toml` does not define is added only when the operator
  opts in:

  ```toml
  [mcp]
  workspace_servers = "off"        # default: a workspace adds no server
  # workspace_servers = "http_only" # HTTP servers (not to a private address)
  # workspace_servers = "stdio"     # HTTP and stdio servers
  ```

  Widening the opt-in is a high-risk configuration change (`stdio`: a
  workspace can then start programs that run as PRX's user). An added stdio
  server must use a bare launcher command from the fixed list (`npx`,
  `node`, `python`, `python3`, `uvx`, `uv`, `deno`, `bun`, `docker`,
  `cargo`, `go`, `ruby`, `php`, `dotnet`, `java`) and starts in the cleared
  environment above. Its `env` cannot set, in any letter case: a
  dynamic-loader variable (`LD_*`, `DYLD_*`, `_RLD*`, `LDR_*`,
  `GLIBC_TUNABLES`, `LIBPATH`, `SHLIB_PATH`, `GCONV_PATH`); an interpreter,
  runtime or package-manager variable that loads code or chooses where the
  launcher fetches it (`PYTHON*`, `NODE_*`, `PERL5*`, `PERLLIB`, `RUBY*`,
  `BASH_FUNC_*`, `NPM_CONFIG_*`, `UV_*`, `PIP_*`, `CORECLR_*`, `COREHOST_*`,
  `DOTNET_*`, `CARGO_*`, `GEM_*`, `BUNDLE_*`, `BASH_ENV`, `ENV`, `CDPATH`,
  `SHELLOPTS`, `PS4`, `JAVA_TOOL_OPTIONS`, `_JAVA_OPTIONS`,
  `JDK_JAVA_OPTIONS`, `CLASSPATH`, `PHPRC`, `PHP_INI_SCAN_DIR`, `GOFLAGS`,
  `GOTOOLCHAIN`, `GOPROXY`, `RUSTC`, `RUSTC_WRAPPER`,
  `RUSTC_WORKSPACE_WRAPPER`); `PATH`; a configuration lookup directory
  (`HOME`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `XDG_*`); or a
  certificate or proxy setting (`SSL_CERT_FILE`, `SSL_CERT_DIR`,
  `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE`, `HTTP_PROXY`,
  `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`), and further (defence in depth, since
  an opted-in stdio server's arguments are code anyway): `DENO_*`, `BUN_*`,
  `RUSTUP_*`, `RUSTFLAGS`, `GIT_*`, `SSH_ASKPASS`, `DOCKER_*`, `GOENV`,
  `GOPATH`, `GOROOT`, `GOSUMDB`, `GONOSUMDB`, `GONOSUMCHECK`, `GOINSECURE`,
  `GOPRIVATE`, `COMSPEC`, `PATHEXT`, `SYSTEMROOT`, `WINDIR`, `HOMEDRIVE`,
  `HOMEPATH`, `PROGRAMDATA`. Such variables are dropped with a warning naming
  them.

`prx doctor` also lists every unfinished configuration change (a `pending`
or `not_restored` history record) with the `prx config rollback` line.

`prx doctor` (category `mcp`) and the `mcp_status` tool (`workspace_servers`)
list every workspace entry that did not start as written: a server blocked by
`mcp.workspace_servers` (with the exact line that would allow it), a
configured server the workspace disabled, a blocked launcher or private URL,
and dropped variables. They name servers and transports only.

**Upgrading (breaking):** a workspace `mcp.json` server that `config.toml`
does not define no longer starts. There is no automatic migration, because
allowing it is the operator's high-risk decision. Either define the server in
`config.toml`, or add to `config.toml`:

```toml
[mcp]
workspace_servers = "stdio"      # HTTP and stdio servers: a workspace can then start programs that run as PRX's user
# workspace_servers = "http_only" # HTTP servers only
```

### Workspace command hooks

A workspace `hooks.json` defines command hooks: programs PRX starts as its
own user on lifecycle events. The workspace file tools can write that file,
so it is not an operator surface, and its command hooks run only when the
operator opts in:

```toml
[security]
workspace_hooks = "off"   # default: command hooks in hooks.json do not run
# workspace_hooks = "on"  # run them (a workspace can then start programs that run as PRX's user)
```

Turning it on is a high-risk configuration change. The setting is read at
every event, so turning it off stops hooks at once. Hooks run in the background, off the turn path (queued, delivered in order,
dropped and counted when 1024 events are waiting). Hooks owned by a
sub-agent (`delegate`, `sessions_spawn`) never run workspace command hooks.
Delivery of each event, across all command hooks, WASM hooks and event-bus
subscribers, has a shared 300-second bound. At shutdown, queued delivery gets
up to three seconds to drain; individual command timeouts still apply.

A command hook that runs:

- starts from the cleared launch environment of MCP servers (`PATH`, `HOME`,
  `LANG`, `LC_*`, `TMPDIR`, on Windows also `SYSTEMROOT`, `USERPROFILE`,
  `TEMP` and `TMP`), plus its own `env` from `hooks.json` and the
  `ZERO_HOOK_*` event variables. No provider key or other variable of PRX's
  environment reaches it. Its `env` cannot set a loader, interpreter,
  search-path, configuration-lookup or certificate/proxy variable (the list
  under "MCP server launch environment"); such variables are dropped with a
  warning naming them.
- starts in a dedicated empty directory, `hooks/workspace-hooks-<hash>`
  beside `config.toml` (owner-only; a symlink or a file at that place is
  refused), never in the workspace. A `cwd` field in `hooks.json`
  is refused when the file is loaded (the whole file is then invalid until it
  is removed). A relative `command` that contains a path
  separator (`./scripts/notify.sh`) is resolved against the workspace; a bare
  name is looked up in `PATH`. Without a configuration directory, command
  hooks are not started.

Writing or editing `hooks.json` or `mcp.json` at the workspace root, or any
file under `<workspace>/plugins/` (WASM plugin code and manifests), with the
file tools is a high-risk operation of its own (`write-launch-definition`,
`edit-launch-definition`); git operations that change those paths are treated
the same way. Every other surface classifies a `hooks.json`
change the same way: a change that installs, changes or runs a hook command
(its program, arguments or event) is high risk (`hooks_manage` `replace` and
`test`, Gateway `POST /api/hooks` and a `PUT /api/hooks/{id}` that sets
`command`, `args` or `event`); one that only removes hooks or changes a
timeout is medium risk (`hooks_manage` `remove`, `DELETE /api/hooks/{id}`, a
timeout-only `PUT`). At `supervised` autonomy a Gateway request carries no
approval grant, so these Gateway writes need `full` autonomy.

`prx doctor` (category `hooks`) and the `hooks_status` tool (`runs`,
`blocked`) report command hooks that do not run, with the one line that
allows them.

**Upgrading (breaking):** workspace `hooks.json` command hooks no longer run
unless `config.toml` holds

```toml
[security]
workspace_hooks = "on"
```

There is no automatic migration, because allowing them is the operator's
high-risk decision. Hooks that run now start in the cleared environment and
the dedicated directory above: a hook that relied on an exported variable
must list it in its `env` in `hooks.json`, and one that relied on starting in
the workspace must use absolute paths or change directory itself.

### WASM plugins

`[plugins] enabled` (default `false`, read at process start) is the only switch.
While it is off, `<workspace>/plugins` is not scanned and no plugin code is
compiled or run. A WASM provider or memory backend named `wasm:<name>` then
fails to resolve like any unknown backend.

With the switch on, a plugin runs only if the operator trust file
`plugin-trust.json` holds a record for it. The file lives in the directory of
`config.toml` (not in the workspace), has mode `0600`, and binds each plugin's
name, the SHA-256 of its component, the SHA-256 of its `plugin.toml` and the
exact permissions granted. A plugin without a matching record is listed with an
error status and never compiled. PRX does not enable plugins when the
configuration directory lies inside the workspace, and it trusts nothing from a
trust file that is a symlink, group- or world-accessible, or malformed.

Records are written by `wasm_plugins_manage install|update`, a high-risk
operation whose approval names the plugin, both hashes and the
`grant_permissions` list. See `docs/plugin-developer-guide.md` for the manifest
side and `docs/plugin-runtime-lifecycle.md` for the admission rules. Compiled
component caches are kept in `plugin-cache/` beside the trust file.

**Upgrading (breaking):** plugins in `<workspace>/plugins` no longer load
unless `[plugins] enabled = true` is set and each plugin has been installed
(trusted) through `wasm_plugins_manage`. The old `.prx-managed` marker no longer
confers trust, an empty `http_allowlist` now denies every URL instead of
allowing all, and allowlist entries are URL patterns rather than string
prefixes (`https://api.example.com*` must become `https://api.example.com/*`).

### Configurations that no longer load

These values were accepted before and are now refused when the
configuration loads. The daemon does not start, `prx config show` fails, and
the Gateway configuration API answers every change with
`baseline_unavailable` until the value is fixed. Each error names the
setting, never a secret.

| Setting | Before | Error (contained in the load error) | Fix |
| --- | --- | --- | --- |
| `autonomy.scopes.default` other than `allow` or `deny` (after trimming, any case) | read as allow unless it was `deny` in any case with no padding (a padded `" deny"` read as allow) | `Failed to deserialize merged config: autonomy.scopes.default must be "allow" or "deny"` | set `default = "allow"` or `default = "deny"` |
| `user_policies[N].role` other than `owner`, `member`, `guest`, `anonymous` | read as `guest` | `user_policies[N].role must be one of owner, member, guest, anonymous` | use one of those roles |
| `user_policies[N].visibility_ceiling` other than `system`, `owner`, `private`, `user`, `group`, `project`, `public` | read as `private` | `user_policies[N].visibility_ceiling must be one of system, owner, private, user, group, project, public` | use one of those values |
| `tunnel.provider` empty, padded, in another case (`"None"`) or unknown | `""` meant no tunnel but passed the public-bind guard; other values failed when the tunnel started | `tunnel.provider must be one of none, cloudflare, tailscale, ngrok, custom` | write the exact lowercase name, `provider = "none"` for no tunnel |
| `a2a.card_jwks_uri` carrying `user:password@` | published in the Agent Card as written | `a2a.card_jwks_uri is published in the public Agent Card and must not carry user:password@` | remove the userinfo from the URI |
| two `cost.prices` keys that name the same provider and model once their URL userinfo is redacted (two credentials on one endpoint) | loaded; a turn's price depended on which key matched | ``cost.prices: the keys `…` and `…` name the same provider and model once their URL userinfo is redacted, so a turn's price would be ambiguous. Keep one key per endpoint and model (prefer a key without credentials; put them in `api_key`).`` (the keys appear redacted) | keep one key per endpoint and model, preferably one without credentials |
| a negative or non-finite `cost.prices` input, output, cache-write or cache-read amount | loaded; mixed signs could cancel into a free usage estimate | `cost.prices entry N FIELD must be finite and nonnegative` (no price key or URL credential is printed) | set every component to a finite amount of at least zero; remove an entry if pricing is unknown |
| `security.audit.max_size_mb = 0` | rotated the audit log on every record, keeping only the last few | `security.audit.max_size_mb must be at least 1` | set 1 or more (the default is 100) |

Two `[[model_routes]]` entries with the same `hint`, or a `hint` containing
whitespace, are refused at load too; see "Model descriptors and the
resolver" in [providers.md](providers.md).

The audit log is written only inside the workspace directory. A
`security.audit.log_path` that is empty, absolute, contains `..`, or runs
through a symlinked directory out of the workspace is not replaced by
another path: audit records are then not written, and each skipped record
logs an error. When a record is written, the log's directory is opened
again from the workspace one directory at a time without following a
symlink, and the log file, its `.lock` sidecar and rotation are handled
relative to that open directory, also without following a symlink. On Unix
this uses directory handles and `O_NOFOLLOW` at every step, so a directory
on the way that is (or is swapped for) a symlink never redirects a record:
if any component or either file is a symlink, dangling or not, the record
is skipped with an error. On other platforms the directory is resolved
again by path, which leaves a short window between that check and the
open. On Unix a `log_path` through any symlinked directory, even one that
points back inside the workspace, can never be written: configuration load
warns, naming the link, and every record is skipped with an error until
`log_path` names the real directory. It is a warning rather than a load
error because the agent can write the workspace, and a link it plants must
not stop the configuration from loading. The log file and its `.lock`
are opened non-blocking and must be regular files with exactly one link: a
FIFO, a device or a hard link to a file elsewhere at either name is refused
the same way (the record is skipped with an error, and a FIFO no longer
stalls every audit writer). Previously such a path fell back to `audit.log` in
the workspace without these checks, and a symlink at that name could send
records outside. Audit records carry no tamper evidence: no audit signing
is implemented, and `security.audit.sign_events`, which never had an effect,
is removed (a config that still sets it loads with a warning; see the
removed keys above). The log lives in the workspace, which the agent's own
OS user can write, so it is a record of what PRX did, not evidence against
an agent that rewrites it.

Offline, edit the file and start again. With the daemon running, fix it
through `POST /api/config` (it merges onto the loaded configuration) or the
raw file API, confirming the `baseline_unavailable` change with
`?confirm_high_risk=true` and the `If-Match` revision from the `428`.

### Configuration transactions, history and rollback

Every configuration write that the running process activates goes through one
transaction layer (`src/config/manage/`): `POST /api/config`,
`PUT /api/config/files/{name}`, `PUT /api/config/secrets`, the agent
`gateway` tool's `config.patch`, and the `prx providers`, `prx models`,
`prx routes` and `prx config` management commands (see
[Managing providers and models from the command line](#managing-providers-and-models-from-the-command-line)).

1. **Validate first.** The change is rendered in its stored form (a
   structured change through the encrypting save, keeping the `config.d`
   layout), staged in a private directory, validated, and loaded exactly as a
   reload loads it, including decrypting every `enc2:` value with this
   installation's key. The same step refuses a change that introduces the
   mask `***` at a secret path or the placeholder `[redacted]@` in a provider
   or endpoint URL or a `cost.prices` key (it would replace real URL
   userinfo), or, with `secrets.encrypt = true`, plaintext at an encrypted
   slot (a value already stored that way does not block an unrelated
   change). A raw file write is checked both as the loader will merge it and
   as the file itself, so a `config.d` file the loader skips or a section
   another file overrides cannot hold such a value either. A change that
   fails any of this is refused with `400` and nothing is written.

   Every model selection the change affects is then resolved, locally, the
   way a surface resolves it before it builds a provider. Selections are:
   for chat, the default, each `[[model_routes]]` destination, each agent
   binding, each `reliability.fallback_providers` member (serving the
   default model) and `reliability.model_fallbacks` entry, each
   `[[router.models]]` candidate, the automix premium model, each
   task-routing `model_hint` and `sub_agent_model`, and the smart-group
   classifier; for embeddings, each `[[embedding_routes]]` destination and
   the memory embedder (`memory.embedding_provider`/`embedding_model`); for
   audio, `media.audio_model` (checked for shape only: PRX has no
   transcription evidence). A selection is affected when the change adds or
   alters it; every selection is when `[providers]` (the
   `[[providers.models]]` declarations) changes; and every selection that
   names a route (`hint:<x>`) is when the routes change, so removing or
   renaming a route that the default, an agent, a task-routing `model_hint`
   or the automix premium model still names is refused (a task rule or the
   premium tier is not read as a raw model once its route is gone). A
   fallback provider must be able to serve the default model by name
   prefix, so changing the default model to one a
   `reliability.fallback_providers` member cannot serve is refused.
   A model the evidence rules out for its operation (for example a route to
   a model declared `operations = ["embedding"]`) is refused with `400`, and
   the answer lists every refused selection by name. On the operator's surfaces
   (`POST /api/config`, the raw file API, the credential endpoint, rollback)
   a model the evidence neither proves nor refutes is accepted and listed in
   the response's `unverified` field; a change made by a model
   (`config.patch`, `proxy_config`) refuses it until it is declared or its
   catalog metadata confirms it. An unchanged selection that is re-checked
   because the declarations changed is refused only on negative evidence.
   A rollback resolves the selections it would restore before it asks for
   `restore_credentials`, `disable_encryption` or `confirm`.
2. **Confirm high risk.** A change that writes a credential (including
   `user:password@` in a provider or endpoint URL, or in a `cost.prices`
   key), turns `secrets.encrypt` off, or changes an endpoint a credential is
   sent to is refused with `428` and the reasons (paths only, userinfo
   redacted) until the caller confirms it. Endpoints are `api_url`,
   `base_url`, a `custom:` URL in any provider field (default, fallback,
   route, agent, embedding, classifier), a new stdio MCP server or a changed
   `command`, `args` or `transport` of one (the program runs as PRX's user,
   so it can read the stored credentials, and it receives its `env` values;
   this holds whether or not it has any `env`, D-045), and any change of an
   MCP server `url`, a remote
   node `endpoint`, the Mattermost `url`, Matrix
   `homeserver`, Signal `http_url`, WhatsApp `ws_url`, Nextcloud Talk
   `base_url`, IRC `server`, email `imap_host`/`smtp_host`,
   `observability.otel_endpoint`, `chat.daemon.url` or a proxy URL
   (`proxy.http_proxy`, `proxy.https_proxy`, `proxy.all_proxy`: every
   credentialed request goes through it). One registry,
   `secret_paths::ENDPOINT_PATHS`, lists these endpoints and the URL fields
   whose userinfo is redacted. A change of an MCP server `env` value
   (`process_environment_changed`) or of a secret that admits a client
   (`access_grant_changed`: the paired tokens, the webhook token and signing
   secret, a channel's verify token or webhook secret, the node server's
   bearer token and HMAC secret; the registry records the kind of every
   secret path, `secret_paths::SecretUse`) is high risk on every surface.
   So is a change of a setting that decides who is admitted inbound
   (`access_grant_changed`, D-049; `secret_paths::ACCESS_CONTROL_PATHS`):
   `gateway.require_pairing`, `gateway.trust_forwarded_headers`,
   `gateway.allow_public_bind`, `gateway.host`, `mcp_server.require_auth`,
   the MCP JWT material and policy (`jwt_public_key_pem`, `jwt_jwks`,
   `jwt_jwks_uri`, `jwt_issuer`, `jwt_audience`, `jwt_algorithms`,
   `allowed_origins`), the A2A peer trust (`allowed_peer_issuers`,
   `trusted_trust_domains`, `spiffe_trust_bundle_pem`,
   `spiffe_strict_validation`), `identity_bindings`, every channel's sender
   allow-list (`allowed_users`, `allowed_contacts`, `allowed_from`,
   `allowed_numbers`, `allowed_senders`, `group_allow_from`), the Signal and
   WhatsApp `dm_policy`/`group_policy`, and
   `channels_config.wacli.allow_unsigned_loopback`. Some settings are
   direction-aware: only their widening direction is high risk, and the
   narrowing direction stays low.
   - A channel's listening scope (`discord.guild_id`, `slack.channel_id`,
     `mattermost.channel_id`, `matrix.room_id`, `signal.group_id`,
     `irc.channels`) moved, dropped or given a new channel is
     `access_grant_changed`, unless the channel's sender allow-list is
     narrower than any scope: it has no `"*"`, and names senders (or is
     empty, where empty admits nobody; Signal's empty `allowed_from` does
     not count, because its DM and group policies decide).
   - `privilege_widened`: a `user_policies` entry granting a higher role, a
     project, a higher `visibility_ceiling` or fewer `blocked_patterns` (a
     removed entry is compared with the default guest policy); an
     `autonomy.scopes` change that is not provably narrower (`default` from
     `deny`, an added `assign_owners` entry, a dropped deny entry, a
     removed, reordered or re-matched rule, an appended rule that carries
     an allow list or is appended under `default = "deny"`, and each allow
     list by what it means at runtime (D-050): `tools_allow` emptied (every
     tool) or given a new tool; `send_allow` given any entry, from empty
     too (the first non-empty `send_allow` is the whitelist, and without one
     only the reply channel is reachable), or emptied (the decision falls
     to a later whitelist or the reply channel); `assign_allow` given an
     entry, or emptied while a later rule that can match the same requests
     carries one (with none, the request falls through to deny, which
     narrows)); a tool added to `mcp_server.exposed_tools`;
     `nodes.server.tls_required` turned off; `autonomy.level` raised,
     `autonomy.workspace_only` turned off, an `autonomy.forbidden_paths`
     entry removed; a `nodes.server.allowed_commands` entry added or a
     `nodes.server.blocked_commands` entry removed; `tunnel.custom.start_command`
     set or changed (a command the daemon runs, like an MCP launch);
     containment (D-051): `runtime.kind` leaving `docker`,
     `runtime.docker.network` reaching further (`none` to any network, a
     named network to another, anything to `host`), a tool newly offered
     to channel sessions (judged on the effective channel surface, where
     `always_exclude` wins everywhere and `always_include` overrides only a
     `channel_exclude` name: a `tool_tiering.always_exclude` or
     `channel_exclude` entry removed, or an `always_include` entry that
     re-admits a `channel_exclude` name),
     `security.audit.max_size_mb` lowered or `security.audit.log_path`
     moved, `runtime.docker.mount_workspace` turned on,
     `runtime.docker.read_only_rootfs` turned off, a
     `runtime.docker.allowed_workspace_roots` root added or the list
     emptied (empty mounts any workspace), and a `browser.allowed_domains`
     entry added (a wildcard included) or the list emptied (empty reaches
     any public host; entries are compared the way `web_fetch` normalises
     them).
   - `access_grant_changed` also covers tunnel exposure: `tunnel.provider`
     switched to anything but `none`, and `tunnel.tailscale.funnel` turned
     on.
   - `remote_decision_enabled`: decision state is sent off this machine,
     or to a different remote backend. It is raised when
     `causal_tree.backend.remote_enabled` is turned on, when
     `causal_tree.enabled` is turned on while a remote decision backend is
     configured, and, while decisions are sent remotely, when the backend
     `kind`, `endpoint`, `model` or `credential_ref` changes. Turning remote
     off, selecting `builtin`, and changes to a loopback backend are low. The
     decision endpoint also receives the backend's credential, so a new
     endpoint is `endpoint_changed`, and a new `credential_ref` is
     `credential_changed`. `config.patch` never changes `[causal_tree]`; a
     hand edit of the files is applied by a reload with a warning.
   - `causal_tree.backend.endpoint` is the decision backend's base URL.
     Compatible HTTP and TypeSafe requests go to `{endpoint}/v1/systemone`.
     Leave `/v1` off (for example `http://127.0.0.1:8900`); an endpoint
     that already ends in `/v1` or `/v1/systemone` is accepted without
     doubling the route. OpenRouter uses `{endpoint}/api/alpha/decisions`;
     an override ending in `/v1` keeps that path segment.
   - Before a decision request leaves the machine, it is checked for every
     secret value of the configuration and for the decision backend's
     credential, in their raw and JSON-escaped forms, and refused if it
     carries one. A value shorter than 8 characters is not looked for (it
     would match ordinary text and refuse every request); the load warns
     once per generation, naming the setting that holds it, never the value.
   - `autonomy.scopes.default` is read once, at load: `allow` or `deny` in
     any case and padding, stored canonically, and anything else is refused
     (the runtime and the plan read the same value). So are the
     `user_policies` `role` (`owner`, `member`, `guest`, `anonymous`) and
     `visibility_ceiling` values, and `tunnel.provider` must be one of
     `none`, `cloudflare`, `tailscale`, `ngrok`, `custom` exactly.
   - `baseline_unavailable`: when the stored configuration exists but does
     not load, a change cannot be compared with what it replaces, so every
     change is high risk until it loads again; repairing it needs the
     confirmation.
   Bind addresses that still authenticate every request
   (`mcp_server.bind`, `webhook.bind`, `a2a.bind`,
   `channels_config.wacli.webhook_listen`) stay low. A change of
   `storage.provider.config.db_url` re-points memory storage and is an
   endpoint change. Confirm with
   `?confirm_high_risk=true` on `POST /api/config` and
   `PUT /api/config/files/{name}`, or `"confirm_high_risk": true` for
   `config.patch`. The `428` body carries the `revision` the refused change
   was computed against. The confirmation is bound to it (D-049): the
   confirmed retry must send it as `If-Match` (the `revision` argument for
   `config.patch`); without it, or with `If-Match: *`, the answer is `428`
   again with `required: "if_match"`, and a change in between answers `409`
   instead of applying risks nobody saw. `POST /api/config` also honours an
   `If-Match` on an unconfirmed change: another revision answers `409`.
   The raw file API accepts only plain lowercase ASCII file names
   (`[a-z0-9][a-z0-9._-]*.toml`, no `..`, at most 255 bytes) and refuses
   every other name with `400` before touching the filesystem: on a case-insensitive filesystem a name such as `Security.toml`,
   `ſecurity.toml` (long s) or `networK.toml` (Kelvin sign) is the managed
   file itself, so a raw write can reach a managed file only under its own
   name, where these checks apply.
   `PUT /api/config/secrets` names one credential slot, so the request
   confirms rotating that slot's outbound credential and nothing else: an
   MCP `env` slot, an admission secret, and any other high-risk difference,
   answers `428`
   and takes `?confirm_high_risk=true` with `If-Match`. The Console raw editor sends the
   revision it loaded the files at as `If-Match` on every save (a change
   made after loading answers `409`), and asks before retrying a `428` save
   with the confirmation and the `428`'s revision (an older embedded Console
   build shows the `428` message instead). For `config.patch`, the
   confirmation is an argument the model sets: at
   `autonomy.level = "supervised"` the approval prompt shows it; at `full` no
   human sees it, so run supervised when an agent may change credentials or
   endpoints. The approval level each surface requires is unchanged.
   The agent `proxy_config` tool's `set` is a configuration transaction
   (D-050: history record, revision check, restore on refusal) and is
   classified by the same plan (D-049): the plan's risk is its
   approval tier (High for a new or changed proxy URL, since every
   credentialed request goes through it; Medium otherwise), and its output
   reports `risk` and `risk_reasons`. At `supervised` a High `set` needs a
   runtime approval grant. At `full` autonomy the gate approves every
   resource operation, High included, without asking anyone (D-037): the
   classification is then recorded, not enforced. The same holds for
   `config.patch`, whose confirmation the model itself supplies at `full`.
3. **No lost update.** Every change is based on a revision (the fingerprint of
   `config.toml` and every `config.d` fragment). `POST /api/config`, the
   credential endpoint and `config.patch` use the revision of the
   configuration they merged into; the raw editor sends the `ETag` of
   `GET /api/config/files` as `If-Match` (a strong validator; `W/` is
   refused), and without `If-Match` the revision read when the change was
   planned is used. If the files changed since (another writer, or a manual
   edit that has not been reloaded yet), the answer is `409` and nothing is
   written; read again (`POST /api/config/reload` picks up a manual edit) and
   retry. On a deployment without the file watcher, every manual edit needs
   that reload before the next API write. The check runs under the same file
   lock every PRX writer takes, so it also holds between two processes.
4. **Recorded, then committed.** Before any file changes, the exact stored
   content of every file the change touches is recorded, sealed, in
   `.config-history/<id>/before.json`, with a redacted `record.json`. The
   files are then written with the existing crash-safe commit (temporary
   file, fsync, rename, odd/even generation barrier and journal): a crash
   leaves the old or the new configuration, never a mix.
5. **Activated or restored.** The new configuration is activated at once. If
   the running process refuses it (a runtime component cannot use it), the
   previous files are put back and re-activated, and the answer is `422`
   "Not applied: … The previous configuration was restored." with the
   `record_id`. If the previous files are back but re-activating them fails,
   or they could not be put back because another writer changed them, the
   answer is `500` and the message says which. A refusal is per process: a
   second PRX process on the same directory may briefly load the refused
   files before they are restored.

`.config-history/` sits next to `config.toml` (directory `0700`, re-applied
when it exists, files `0600`). Each before-image is sealed with this
installation's secret key (`.secret_key`), whatever `secrets.encrypt` says,
so no credential in it (encrypted slots, channel tokens, MCP environment
values, or plaintext written with encryption off) is readable at rest;
replacing the key makes the old records unreadable. The file watcher ignores
the directory. The newest 20 finished records are kept; a `pending` or
`not_restored` record is never removed by retention. Record states:
`pending` (the process stopped before the transaction finished: disk holds
either version, nothing is applied automatically), `commit_failed`,
`activated`, `activation_deferred` (written by a `prx` command; a running
PRX's watcher settles it, see below), `rolled_back`, `not_restored`,
`superseded` (a `prx` change a later write replaced before a running PRX
settled it; nothing was restored, and its before-image stays restorable),
`credential_replaced` (a credential reference's stored value was replaced;
not restorable, because the previous value is not kept: `prx config
rollback` refuses it and `--last` skips it). If the history directory cannot
be written (not a directory, a symlink, a full disk), every write through the
transaction layer fails closed with nothing written.

A rollback restores the files **to how they were before** a recorded change,
which also undoes every later change to those files. It is a new transaction
with the same validation and boundary checks, and it is refused, with
nothing written, unless the caller confirms it and names the current
revision. It additionally needs an explicit flag to bring back a credential
the current configuration no longer holds (a key rotated out after a leak,
or a `user:password@` in a provider URL that was changed since) or to turn
`secrets.encrypt` off, and a record whose before-image this
installation's key cannot unseal or decrypt is refused. On the command line:
`prx config history` lists the records, `prx config history show <id>` shows
one (its redacted plan and reason), `prx config rollback <id>` or
`prx config rollback --last` shows what is undone and rolls back with
`--yes` (plus `--restore-credentials` or `--disable-encryption` when the
refusal names them), and `prx config status` shows the revision, how the
default model resolves, and any `pending` or `not_restored` record. The
Gateway exposes authenticated `GET /api/config/history` and
`POST /api/config/rollback`; rollback retains revision and confirmation checks.
See [provider management](provider-management.md) for the management interfaces.

**The decision backend check.** Every plan, rollback and reload checks the
`[causal_tree]` decision backend: its model must be declared for
`decision_endpoint`, and a `credential_ref` must exist and belong to the
backend's provider. The hosted services (`kind = "typesafe"` or
`"openrouter"`) take only a `cred-` reference owned by the service
(`decision:typesafe` or `openrouter`, never the root key), one exact
`model` version, and for OpenRouter the exact dated snapshot it answers
with in `pinned_snapshot`; any other answering model falls back to the builtin.
When a managed backend change selects a release-catalog decision model and
omits the snapshot or timeout, the same transaction writes the catalog's pinned
snapshot and recommended timeout. It also writes the catalog's recommended
extra-latency budget when the policy still has its default value. Explicit
command and policy values remain operator choices. Built-in decision-model
pricing comes from the catalog; `[cost.prices]` is only an override.
Automatic Router candidates and doctor use those configured price overrides
when filtering active release rows with Router metadata.
During migration, only schema-1's serialized 500 ms default becomes implicit.
An unversioned pre-1.7 file used 300 ms as its default, so its stored 500 ms
remains an explicit operator choice, as does 500 ms in schema 2. Loading an
already-configured hosted backend does not raise its current ceiling: when it
is below the model's catalog recommendation, configuration loading logs a
warning, `prx causal status` shows it (also as `latency_budget_warning` in JSON),
and `prx doctor` reports it. Set the policy explicitly to adopt a larger budget;
the warning itself changes neither admission limits nor backend credentials.
An `endpoint` replaces a hosted service's base URL, and the backend stays
remote, so it is used only with `remote_enabled`. The hosted credential
belongs to the service, not to the URL: re-pointing the override sends that
credential to the new host. The change is high risk (`endpoint_changed`),
so it needs the confirmation that names the revision, and nothing is sent
while `remote_enabled` is off. While the plane is
disabled, the credential is not resolved and the credential store is not
opened. Only a change that owns a problem is refused: one that
touches `[causal_tree]`, or one that makes a backend that passes the check
fail it (removing its model declaration, for example). A change elsewhere
that finds the section already refused, or the credential store unreadable,
is applied with a warning, and the decision plane runs builtin-only until
the section is fixed. Startup never fails for it either: `prx doctor` and the
startup log say that the plane runs builtin-only, and why. `prx doctor` also
lists decision credentials (`cred-decision-...`) that nothing references,
which a process stopped between storing a credential and applying it can
leave; a rotation removes the credential it replaces. A calibration
(`[causal_tree.policy.calibration_profile]`: `kind`, `endpoint`, `model`)
holds only while the backend's kind, endpoint and model all match it.

Decision requests bind the retained backend's configured model before sealing;
the main model remains part of the turn's provenance. A model mismatch is
refused before cache lookup, health probing or HTTP I/O. `pinned_snapshot`
checks the answering version separately from the configured request model.
The Agent turn service continues to exclude configured slots without proven
budget admission.

**Rolling back past a decision credential.** A rollback runs the same check,
so it is refused when the configuration it restores references a decision
credential that was removed since (a rotation removes the one it replaces),
or that now belongs to another provider. To recover, roll back to a record
whose configuration selects the builtin backend (or none), then configure
the backend and store a new credential; nothing is sent while the builtin
is selected.

**After a credential leak**, rotate the credential, then purge the
configuration history: it still holds the old value (sealed) in up to 20
before-images. The purge the management surfaces will offer takes the same
two locks a change holds while it records and commits (the process's
transaction lock and the configuration writer lock), so it never removes a
record while that record is being created. A change in another process
updates its record once more after activation, outside the writer lock; a
purge that lands in between removes the record, and that last update is
lost and logged, nothing else. The command is
`prx config history purge --yes`.

Each record and each successful response carries the risk class (`low` or
`high`) and its reasons.

Writers not yet on the transaction layer (they still save through the
validated, crash-safe commit, but without the revision check, history and
restore): pairing-token persistence and the
Telegram in-channel bind (each reloads right after saving), and the offline
CLI writers `prx channel bind-telegram`, onboarding, `prx config split` and
`prx config merge` (a running daemon picks their change up through the file
watcher). They take the same file lock, so a transaction never overwrites
their change (it answers `409`), but they can overwrite a transaction's.

### Managing providers and models from the command line

The management commands call the same service the Gateway uses, in-process:
the same validation, the same plan and the same refusal text for the same
change. Every mutating command prints its plan and writes nothing without
`--yes`; a high-risk plan (a credential, an endpoint, a stdio MCP launch, …)
also needs `--confirm-high-risk <REVISION>`, where the revision is the one
the plan printed, so a change made in between is refused (`409`-style)
instead of being confirmed unseen. The CLI writes through the transaction
(revision check, history record, load validation); a running PRX picks the
change up through its configuration watcher. An operator's command accepts a
model PRX cannot verify and lists it under `Unverified:`; a model the
evidence rules out is refused.

| Command | What it changes |
| --- | --- |
| `prx providers` / `prx providers list` | nothing: the supported providers, then the configured ones with their roles and every selection that uses them |
| `prx providers show <provider>` | nothing: identity, roles, uses, declarations, and where its credentials are (never a value) |
| `prx providers add <provider> --fallback` | appends it to the failover chain, `reliability.fallback_providers` (the role is required; the default is set with `prx models default set`) |
| `prx providers update <provider> --api-url <url>` / `--clear-api-url` | `api_url`, which only the default provider uses (high risk) |
| `prx providers remove <provider>` | removes its failover membership and its `[[providers.models]]`/`[[providers.tool_dialects]]` rows (not its auth profiles or `cred-` references); refused while any selection (the default, a route, an agent, an embedding route, the memory embedder, …) uses it, naming each |
| `prx providers test [<provider>] [--model <m>] [--live]` | nothing: resolves locally; `--live` sends one model-list request, never a generation, to the host provider construction addresses: a configured `api_url` for the adapters that honour it (`openai`, `ollama`, llama.cpp, LiteLLM, vLLM, the Qwen OAuth aliases), else the provider's own endpoint. `prx models refresh` and `POST /api/providers/{id}/test` use the same host |
| `prx providers credential set <provider> [--name <n>] [--from-stdin \| --from-env VAR]` | stores the credential reference `cred-<name>` (default name: the catalog provider's name) in the encrypted auth-profile store; the value comes from a hidden prompt, standard input or an environment variable, never an argument. A model route, embedding route or agent of that provider uses it once it names it in `credential_ref` (see below); an enabled decision backend can use a configured reference. The output also names the key PRX sends the provider when nothing names a reference (`prx auth paste-token --provider <p> --from-stdin`, or the root `api_key`). Replacing an existing reference is recorded in the configuration history (by id, never the value) |
| `prx providers credential remove <name>` | removes it; refused while the stored configuration names it in a `credential_ref`, listing where |
| `prx providers credential status [<provider>]` | nothing: every reference and who names it, or one provider's root connection, inline keys, auth profiles and references |
| `prx providers migrate [--apply [--yes --confirm-high-risk <revision>]]` | without `--apply` nothing: a redacted report of what is legacy (inline keys of routes, embedding routes and agents, the root connection, `[[providers.tool_dialects]]` rows, selections the evidence does not verify or no longer resolves, auth profiles stored under a catalog alias such as `google`, which no provider reads). With `--apply` the plan; with `--yes` it stores each inline key as a credential reference (`cred-<kind>-<name>-<hash>`, owned by the selection's provider), writes the selections with `credential_ref` instead of `api_key`, and moves the dialect rows into `[[providers.models]]`, through the configuration transaction (high risk: it rewrites credentials). `prx config rollback <record> --yes --restore-credentials` restores the files byte for byte; a second run has nothing to migrate. The root connection stays; alias-stored tokens are reported only |
| `prx models add\|update --provider <p> [--model <m>] [--operations chat,embedding] [--native-tools true] …` | one `[[providers.models]]` declaration (update replaces only the facts given) |
| `prx models remove --provider <p> [--model <m>]` | removes that declaration |
| `prx models list --operation <op> [--executable]` / `prx models show <provider> <model>` | nothing: every model PRX knows of (configured selections, declarations, the `prx models refresh` cache) with what the evidence says it can do; `--operation` leaves out models the evidence rules out for it, `--executable` keeps only models the evidence proves can execute it (unverified ones are left out). `prx models list` without these flags prints the cached catalog as before |
| `prx models default show` / `set <provider> <model> [--operation chat\|embedding]` | `default_provider`/`default_model`, or `memory.embedding_provider`/`embedding_model` |
| `prx routes list\|add\|update\|remove [--operation chat\|embedding]` | `[[model_routes]]` or `[[embedding_routes]]`, by hint; removing a route a selection names is refused |
| `prx config agents list\|set <agent> <provider> <model>` | `agents.<name>.provider`/`model` of an existing agent |
| `prx config plan --file <path> [--target <name>]` / `prx config apply … --yes` | one raw configuration file, checked like the raw editor's `PUT` (a TOML error is reported by position only; no message quotes a secret the file carried) |

If `prx providers migrate --apply` would change `[[embedding_routes]]`, make
their hints unique first. An existing configuration with duplicate hints still
loads, but a migration that changes those routes is refused before writing
configuration or storing credential references.

The root connection (`api_key`, `api_url`, `reliability.api_keys`) belongs
to the default provider. `prx models default set` refuses to switch the
default to another provider while it holds anything, unless the command says
`--keep-root-credential` (it now belongs to the new provider) or
`--clear-root-credential` (cleared with the switch, a high-risk change).
Both are refused with `--operation embedding`. On every write path (the
Gateway, the raw editor, `config.patch`, the CLI) a switch of the default
provider while `api_key` or `reliability.api_keys` holds a key, and a route,
embedding route or agent that moves to another provider while it holds its
own `api_key`, is a high-risk change (`credential_owner_changed`): the key
is sent to the new provider.

A `prx` command does not run the configuration's runtime: it validates and
loads the change, writes it, and records it as `activation_deferred`, never
`activated`; the command cannot confirm the activation, and says so. The
record names the revision the command wrote as soon as the files are
written. A running PRX settles the record from what its watcher saw of
exactly that revision: `activated` when its runtime took it; when its
runtime refused it, it restores the configuration from before the change
only while the files are still at that revision, re-activates it and records
`rolled_back` with the reason (`not_restored` if the restore fails). If a
later write already replaced the refused content, nothing is restored and
the record becomes `superseded`; so does any deferred record whose revision
was replaced before a running PRX settled it. When the watcher finds no
record for what it loaded yet, it tries again for a few seconds. A PRX
started on the files marks the record `activated` when its watcher starts.
`prx config status` and `prx doctor` list records still deferred and
superseded ones.

Commands that only read (`prx doctor`, `prx providers` list/show/test,
`prx providers migrate` without `--yes`, `prx providers credential status`,
`prx config status|show|history|plan|schema`, `prx config agents list`,
`prx models list|show`, `prx routes list`) load an existing
configuration read-only and write nothing. Every other command upgrades a
legacy `enc:` value in `config.toml` or a `config.d` fragment when it loads
the configuration, under the transaction lock and only if no other write
landed meanwhile (otherwise at the next start); with `secrets.encrypt =
false` such a value is never rewritten (it keeps loading).

**Credential references in the configuration.** A model route, an embedding
route or an agent can name a credential reference instead of an inline key:

```toml
[[model_routes]]
hint = "fast"
provider = "groq"
model = "llama-3.3-70b-versatile"
credential_ref = "cred-groq-fast"   # prx providers credential set groq --name groq-fast
```

The reference is sent only to the provider it was issued for: a change that
names a missing reference, or one issued for another provider, is refused
when it is planned (a rollback that would restore a reference removed since
included), and at run time such a reference is not sent (a warning names the
id). Setting both `api_key` and `credential_ref` is refused at load. Setting
or changing a `credential_ref` is a high-risk change like writing a key, and
`prx providers credential remove` refuses a reference any of them names.
Replacing a reference's value (`prx providers credential set`, or the
Gateway credential `PUT`) is recorded as `credential_replaced`. A running
consumer sends the new value only if it resolves that reference again. Gateway
agent and tool consumers rebuilt on a configuration change can do so (a reload
of unchanged files, or of restart-only fields, rebuilds nothing); consumers
whose providers were built at startup need a restart. A Gateway configuration
generation and a channel runtime started with `default_model = "hint:<route>"`
use that route's destination and owner-bound credential. Channel per-sender
hint selections and delegated or spawned children resolve their selected route
individually. A direct model keeps its configured provider connection.
Restart PRX to apply a rotation to startup-built consumers.
The `[[providers.tool_dialects]]` table still loads, with a deprecation
warning; `prx providers migrate --apply` moves it into `[[providers.models]]`.

A secret on the command line is visible to other local users in the process
list and stays in the shell history. `prx init --api-key`, `prx go -k`
and `prx auth paste-token --token` still accept one, and print a
deprecation warning on stderr; use `prx init --from-stdin` or
`--from-env VAR`, `prx go --from-stdin` or `--from-env VAR`, and
`prx auth paste-token --from-stdin` (or its hidden prompt) instead.

### Managing providers and models through the Gateway

The Gateway endpoints call the same management service as the commands
above, in-process. The same change gives the same plan text (`plan`) and
the same refusal text (`error`) as `prx`. Every endpoint needs the Gateway
bearer token (or the Console cookie), and read-only autonomy refuses every
write with `403`.

**Autonomy gate.** A write that has a plan passes the resource gate at a
level derived from what the plan changes, not from the endpoint it arrived
at. The same change therefore meets the same gate on every endpoint:
`POST /api/config`, the raw editor (`PUT /api/config/files/{name}`), the
typed endpoints below, `POST /api/config/apply` and rollback.

- **High:** a plan that creates or changes a secret (a provider key, channel,
  webhook or paired token, or MCP environment value), or attaches a stored
  credential reference. This is the same level as `PUT /api/config/secrets`
  and the credential reference `PUT`, so a key meets one gate whichever
  endpoint writes it.
- **Medium:** a plan that only clears secrets. Secrets are compared by
  identity (a route's hint, an agent's name, a list value), so removing a
  route in front of one that holds a key is no secret change.
- **Medium:** a plan that changes the provider set (`default_provider`,
  `reliability.fallback_providers`, `api_url`), re-points a provider
  endpoint, or hands a stored key to another provider.
- **Low:** every other plan.

The credential endpoints have no plan; they are High.

| Endpoint | Does | Gate |
| --- | --- | --- |
| `GET /api/providers`, `GET /api/providers/{id}` | lists the configured providers; shows one provider's roles, uses and where its credentials are (never a value) | read |
| `POST /api/providers` `{"provider"}` | adds a failover provider | from the plan (Medium) |
| `PATCH /api/providers/{id}` `{"api_url": "<url>" \| null}` | sets or clears the default provider's endpoint (a high-risk plan) | from the plan (Medium) |
| `DELETE /api/providers/{id}` | removes the provider; `409` with `referrers` while a selection uses it | from the plan (Medium when it was a failover member) |
| `PUT /api/providers/{id}/credential` `{"value", "name"?}` | stores the reference `cred-<name>`; answers `204` with no body. The request itself confirms it: there is no `428` and no `If-Match` | High |
| `DELETE /api/providers/{id}/credential?name=<n>` | removes the provider's reference; `409` with `referrers` while the configuration names it | High |
| `POST /api/providers/{id}/test` `{"model"?, "live"?}` | tests a configured provider only (`404` otherwise); sends nothing unless `"live": true`, and then exactly one model-list request | read (`live`: Low) |
| `GET /api/models?provider=&model=&operation=&executable=` | the model read model of `prx models list` / `show` | read |
| `POST`/`PATCH`/`DELETE /api/models` (declaration in the body) | `[[providers.models]]` | from the plan |
| `GET`/`PUT /api/defaults` `{"operation"?, "provider", "model", "root_credential"?}` | the chat or embedding default | from the plan (Medium when the default provider changes) |
| `GET`/`POST /api/routes`, `PATCH`/`DELETE /api/routes/{hint}` (`?operation=chat\|embedding`) | `[[model_routes]]` / `[[embedding_routes]]` | from the plan |
| `GET /api/agents`, `PUT /api/agents/{name}` `{"provider", "model"}` | an agent's model binding | from the plan |
| `POST /api/config/plans` `{"change": {...}}` | dry run of any change above (`"kind": "add_route"`, `"set_default"`, …) or a raw file (`"kind": "raw_file"`, with the raw editor's file checks); nothing is written | read |
| `POST /api/config/apply` `{"change": {...}}` | the same change, committed; a raw file answers the raw editor's removed-keys `warning` | from the plan |
| `GET /api/config/history` | the redacted history records with their plan text | read |
| `POST /api/config/rollback` `{"record_id", "confirm", "restore_credentials"?, "disable_encryption"?}` | restores the configuration from before a record. `plan` is what the restore writes (it also undoes every later change); `undoes` is the recorded change | from the restore plan |

The `raw_file` change on `POST /api/config/plans` and `POST /api/config/apply`
does not accept a secret-clear option. To intentionally clear stored secrets,
use `PUT /api/config/secrets`, or use the CLI's `--allow-credential-clear` flag
for a raw-file replacement. The separate raw editor endpoint,
`PUT /api/config/files/{name}`, accepts `?allow_secret_clear=true` as described
above; high-risk confirmation and revision checks still apply.

Rules that hold on every endpoint:

- **Model ids go in the query or the body, never the path.** A model id can
  contain `/` (for example `openai/gpt-4o`). A provider or hint in the path
  is percent-encoded (`custom%3Ahttp%3A%2F%2F127.0.0.1%3A8080%2Fv1`).
- **No credential in a URL.** The request is refused with `400` and nothing
  is stored when the URL carries any of these:
  - URL userinfo;
  - a credential-shaped token;
  - a query parameter the endpoints do not take. They take `provider`,
    `model`, `operation`, `executable`, `confirm_high_risk` and `name`.

  A credential is accepted only in the body of the credential `PUT`.
- **Provider text is redacted.** Every provider an answer names has its
  URL userinfo, query values and fragment redacted. That covers list
  entries, refusals and the test report.
- **Optimistic concurrency.** Every write with a plan takes
  `If-Match: "<revision>"`. It must equal the revision the plan was read
  from, otherwise the answer is `409` and nothing is written.
  - The plan answers, `GET /api/defaults`, `GET /api/providers` and
    `GET /api/config/history` report the current `revision`.
  - A raw file's plan reads the files, so its revision is the files' own.
  - Without `If-Match` a change is applied to whatever revision it was
    planned on.
  - The credential endpoints take no `If-Match`.
- **High-risk confirmation.** A high-risk plan answers `428` with
  `required`, `risk_reasons`, `revision` and the `plan` text. Examples: an
  endpoint change, clearing the root key, or moving the root key to another
  default provider (`credential_owner_changed`). To confirm, resend with
  `?confirm_high_risk=true` and `If-Match` set to that `revision`. Without
  `If-Match`, or with `If-Match: *`, it answers `428` again with
  `required: "if_match"`. A rollback confirms the same way, with its flags
  in the body, each counting only with `If-Match`.
- **Plans are stateless.** There is no stored plan id. Bind a plan to its
  apply by resending the same change with `If-Match` set to the plan's
  `revision`: then any change in between answers `409` instead of applying
  something nobody saw.
- **Switching the default provider.** A switch away from a root `api_key`
  needs `"root_credential": "keep"` or `"clear"`; without one it answers
  `400`. Both choices are high-risk plans: `keep` hands the key to the new
  provider, and `clear` writes the credential.
- **Supervised autonomy.** The resource gate needs a runtime approval grant
  for a Medium or High operation. A Gateway request cannot carry one, so at
  `supervised` these writes answer `403`, and the body's `reason` says so.
  This includes a provider-set change sent through `POST /api/config`, the
  raw editor or rollback. This is the same approval rule as every other
  Gateway write. Use `full` autonomy or the `prx` commands.

## Provider Tool Dialects

`[[providers.tool_dialects]]` declares what an OpenAI-compatible endpoint the
operator runs accepts in a tool schema, and whether its textual
`<function=NAME>{json}</function>` tool-call grammar is read. PRX never infers
either from a URL: an endpoint with no entry is sent the published tool
schemas unchanged (`raw`) and only its native `tool_calls` array is read
(`native`).

```toml
# An endpoint that rejects allOf/anyOf/oneOf at the top level of a tool schema
# (HTTP 400 "unsupported top-level tool schema keyword(s)").
[[providers.tool_dialects]]
provider    = "custom:http://127.0.0.1:8080/v1"
tool_schema = "no_root_combinators"
tool_call   = "xml_function_tags"   # provider-wide entries only

# One model behind the same endpoint that accepts the published document.
[[providers.tool_dialects]]
provider    = "custom:http://127.0.0.1:8080/v1"
model       = "published-schema-model"
tool_schema = "raw"

# A catalog local server, keyed by its catalog name (api_url may point anywhere).
[[providers.tool_dialects]]
provider    = "vllm"
tool_schema = "no_root_combinators"
```

`provider` accepts a `custom:<http(s) url>` endpoint or `vllm`, `llamacpp`,
`lmstudio`, `litellm` (under any alias); every other provider is refused at
load. `tool_schema` is `raw` or `no_root_combinators`; `tool_call` is `native`
or `xml_function_tags`. An entry is matched by provider identity, so spellings
that differ only in case, padding or a trailing slash are the same entry, and
a duplicate entry is a load error. Declare the whole table in one file
(`config.toml` or one managed `config.d/` fragment, that is one of the file
names the split layout uses, such as `routing.toml`; any other file in
`config.d/` is not read): two files declaring it is a load error, because a
fragment's array would otherwise replace the other file's. No managed
fragment owns `[providers]`, so the first structured save (the Web
Console, `POST /api/config`, `config.patch`) moves a table declared in a
fragment into `config.toml` and removes it from the fragment.
`xml_function_tags` is read on non-streaming turns only. A URL with
`user:password@` keeps that credential in this table, as it does in
`default_provider`; output redacts it. The value table, the refusal rules and what
`no_root_combinators` does to a tool that cannot be lowered are in
[providers.md](providers.md#tool-schema-dialects).

## Provider Model Descriptors

`[[providers.models]]` declares what a provider's models can do. PRX reads it
as operator evidence over the embedded [release model catalog](model-catalog.md)
when it checks a selection (see
[providers.md](providers.md#model-descriptors-and-the-resolver)); it never
makes an operation executable that the provider's adapter cannot run.

```toml
# A decision-only model: the resolver refuses it for chat.
[[providers.models]]
provider   = "openrouter"
model      = "vendor/decision-model"
operations = ["decision_endpoint"]

# A local endpoint's model, fully declared.
[[providers.models]]
provider            = "custom:http://127.0.0.1:8080/v1"
model               = "incoai/Qwen3.8-27B-Splash"   # omit to cover every model
operations          = ["chat"]
native_tools        = true
vision_input        = false
streaming           = true
tool_schema_dialect = "no_root_combinators"
context_window      = 131072

# Provider-wide: the endpoint's textual tool-call grammar.
[[providers.models]]
provider          = "custom:http://127.0.0.1:8080/v1"
tool_call_dialect = "xml_function_tags"
```

Every field but `provider` is optional and declares nothing when omitted.
`operations` is the complete list: an operation not listed is unsupported for
that model. `native_tools`, `vision_input`, `streaming` and `structured_output`
are `true` or `false`. A model-scoped entry overrides the provider-wide entry
field by field. A declaration overrides release-catalog facts and positive
discovery evidence. Negative discovery evidence still wins, and the provider
adapter remains the upper bound.

`tool_schema_dialect` and `tool_call_dialect` take the values of
`[[providers.tool_dialects]]` `tool_schema` / `tool_call`, for the same
providers (`custom:` endpoints and `vllm`, `llamacpp`, `lmstudio`, `litellm`),
and `tool_call_dialect` only on an entry without `model`.
`[[providers.tool_dialects]]` stays accepted; a `(provider, model)` may declare
a dialect in only one of the two tables.

The load refuses, naming the entry by index: an empty or unknown provider, an
empty `model`, an entry that declares nothing, an empty `operations` list, an
unknown operation (the error lists the accepted values), `context_window = 0`,
a dialect on a provider that does not read one, a per-model
`tool_call_dialect`, a duplicate `(provider, model)`, and a dialect declared in
both tables. Declare the whole table in one file (`config.toml` or one
managed `config.d/` fragment; the first structured save moves it into
`config.toml`, as for `[[providers.tool_dialects]]`). A `user:password@` in
a `custom:` URL is redacted in every error.

## LLM Router

The candidate's embedded Agent Router supports local heuristic ranking
(capability + Elo + cost + latency). Scoped KNN requires an admitted history
read, an authenticated embedding domain and enough records in the same source
domain. The 2026-10-05 source addendum wires supported configured paid embeddings
and AutoMix premium calls through per-physical-attempt accounting. Merely
configuring a model is insufficient: source/model admission, a supported actual
transport, pinned prices and remaining original token/money budgets are required.
Automix uses its own window starting at the cheap reply:
`router.automix.escalation_window_ms = 30000` by default, with an allowed range
of `1..=120000` ms. Main settlement and premium reservation consume that window;
the 500 ms decision and 100 ms Router budgets are unchanged. See the
[current candidate addendum](router.md#source-addendum-2026-10-05) for scope,
unsupported cases and verification status.

Minimum router config (single reachable provider):

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

For full examples, field-by-field reference, flow, and security boundaries, see [docs/router.md](router.md).

The CLI no longer constructs the legacy speculative CTE engine or runs its separate AskApproval hook. Source-bound turn decisions and their approval records belong to the shared turn decision service. The removed legacy hook does not provide an alternate approval authority; retained v1 report compatibility is separate from runtime execution.

For ordinary CLI single-shot and interactive turns, disabling the causal decision plane still runs the built-in conservative decision and constrained tool selection. Disabled decisions do not create decision/outcome settlement writes or enable remote decision backends; normal conversation history and terminal records follow their existing settings. Scheduled and other ingress migration is tracked separately and is not established by this CLI wiring.

Chat also enters the built-in decision path when the causal plane is disabled. Provider selection remains bound to its runtime generation. If conversation recording supplies a durable inbound event, Chat verifies it; otherwise disabled mode keeps an in-memory source without inventing an event ID. Enabled mode still requires the durable event. Disabled terminal handling omits causal settlement/rejoin writes while retaining normal conversation behavior.

Gateway webhook and platform-message turns also enter the built-in decision path while disabled. Their source remains bound to the admitted transport actor, workspace and runtime generation; optional unrecorded input carries no invented event ID. The shared terminal guard retains cancellation cleanup without causal settlement writes. Transport authorization and outbound delivery checks remain required.

Scoped single-shot agent invocations retain the originating run and session identifiers for terminal records and child lineage. A missing run ID, empty session or mismatched workspace is rejected before runtime bootstrap. A scoped invocation cannot enter the interactive REPL. These identity checks do not replace scheduler lease validation or establish completion of scheduled decision admission.

The Agent runtime publishes its decision service once for its actual CLI, Cron or Xin surface. For a non-operator scheduled run, runtime construction uses the same effective security policy as its scheduled tool adapter, after checking that the source and execution scope identify the same task attempt. Per-turn scheduled source admission remains a separate migration requirement.

When a tool execution carries a runtime authority guard, the shared execution service revalidates it after preparation and reservation, before invoking the backend. Expired or unavailable authority prevents invocation; cancellation also stops dispatch. Failure to abandon a durable reservation is reported as indeterminate. This check does not make a later lease change atomic with an external side effect.

Immediate Cron agent runs retain a Cron envelope even though they have no scheduler lease. Each invocation receives a distinct immediate-run identifier; scheduler-claimed runs retain their claim identifier and authority guard. Both paths preserve stored owner, topic, source-event and owner-isolation metadata and apply the job creator's execution policy. An absent scheduler claim does not grant local-operator authority.

Daemon-managed Cron and Xin agent runs pass the scheduler's pinned configuration generation and shared configuration manager into Agent bootstrap. Cron explicitly carries this context across its spawned job boundary. Model selection and decision publication therefore use that pinned effective configuration, without creating a second manager or reconstructing a revision from a later disk state. Standalone calls keep their startup configuration path. The legacy generation-only Cron API adopts its supplied snapshot without re-reading configuration, checks that its Config argument matches, and continues generation numbering. Snapshots with deferred configuration require the existing-manager API because the effective snapshot cannot reconstruct desired settings. Unavailable source revisions and exhausted generation counters are refused.

Decision source proofs retain an explicit task ID when the ingress supplies one. The decision state, execution binding, approval source and terminal/history correlation must agree on it. Durable task identity comes from a typed Task event subject; topic/goal labels and run IDs are not converted into task IDs. Missing historical task identity is not retroactively assigned.

Cron/Xin agent invocations carry creator admission from the stored job/task row. Before bootstrap, Agent checks task/source lineage and the exact creator-derived execution context; an absent extra tool scope is accepted only for a recorded LocalOperator origin. Claimed Cron and Xin invocations require their matching attempt/lease and a live authority guard. Immediate Cron invocations retain their separate unique identity without inventing a claim. Cancellation or lost claim/lease prevents initialization.

Cron/Xin use the Agent tool-tiering surface together with the stored creator's security policy and owner isolation. A creator channel label alone does not select the IM channel profile. IM turns apply `for_channel_surface()` in their owning adapter; inherited child capability boundaries remain part of child admission. Global exclusions and exact-model allowlists continue to apply to scheduled decisions.

Scheduled Cron/Xin Agent turns retain terminal-cleanup ownership across task cancellation. During daemon shutdown, scheduled cleanup is acknowledged separately from supervisor aborts; an unconfirmed terminal or decision-queue drain returns an error. A replacement Cron/Xin configuration generation waits for the previous generation's cleanup, and new child work is refused after an unconfirmed cleanup failure. Expired task authority is still rejected during terminal persistence. Manual invocations wait for their local cleanup before returning; a forced drop without an enclosing owner cannot acknowledge completion to the caller.

`reliability.scheduler_retries` retries Cron shell attempts and Agent startup failures before shared decision admission. Once an Agent invocation starts decision admission, the scheduler does not automatically replan the entire turn after failure: that turn may already own a durable decision or tool effects. An unconfirmed cleanup also prevents a whole-turn retry. Provider request retries remain governed by the existing provider reliability settings. A claimed invocation keeps its claim/run identity; this rule does not generate a new identity to bypass terminal or tool idempotency records.

Gateway and Console turns also own cleanup for manually invoked scheduled Agent work. Their ordinary result waits for child cleanup; asynchronous jobs retain a separate child-cleanup acknowledgement outside the abortable work task. A lost, failed or timed-out acknowledgement remains an error. This does not grant a manual task scheduler lease or permission to repeat its effects.

Ordinary Agent CLI/REPL execution owns cleanup for manual scheduled child calls before returning, while retaining its existing turn shutdown and decision-queue drain path. Interactive Chat carries its cleanup owner explicitly into spawned provider tasks. Child cleanup completes before the provider task emits its ordered-commit or completed action; unconfirmed cleanup is reported as a non-retryable failure. Chat exit drains retained child cleanup before completing its service shutdown.

Channel message workers explicitly inherit their generation's cleanup owner. A model round waits for manually invoked child cleanup before producing its final channel reply; unconfirmed cleanup bypasses ordinary cancellation and context-overflow retry classification. Closing the inbound stream remains graceful. Explicit component shutdown cancels worker cleanup scopes, and failed cleanup is returned after listeners stop. Daemon channel generation replacement retains the same retirement barrier used for scheduled generations. Message fan-out remains ungated.

Task-mode background workers use independent child-cleanup ownership, so a spawning turn returning does not cancel legitimate background work. Their external monitor retains cleanup after task abortion. Each steering iteration drains its manual child calls before returning history; unconfirmed cleanup prevents starting a replacement iteration. Dropping the outer steering future also aborts its active inner loop instead of detaching it.

Task-mode `sessions_spawn` and `subagents` kill operations wait for the external task monitor to acknowledge exit and child cleanup. A request made before the abort handle is attached is retained and applied when attachment completes. Natural completion wins the race with a late termination request; missing ownership, timeout, panic or unconfirmed cleanup does not report a successful kill. Chat session shutdown requests task termination concurrently and retains pending or unconfirmed task entries instead of counting and clearing them as terminated.

Process session workers also own cleanup for nested scheduled work. Each steering segment waits for child cleanup before accepting a replacement instruction; unconfirmed cleanup ends the run instead of starting another segment. Worker result publication follows a successful terminal commit, and the worker drains its retained cleanup before returning the result. An operating-system process kill cannot run this in-process cleanup path; process termination and durable recovery evidence remain separate.

The staged decision-enabled worker recovery path attaches a recovery
task to each Chat, Channel or Gateway tool generation. It reads at most 128
launch records per page, waits one second between full pages, and retries a
completed sweep or unavailable storage after 30 seconds. Disabling the decision
plane starts no recovery task. Dropping the tool generation cancels its task.
Recovery requires authenticated launch, source, parent decision, terminal and
usage records, plus process absence in the same OS boot context (and Linux PID
namespace). Other hosts, later boots, legacy records without this binding, and
unknown process state remain unconfirmed. Recovery inserts read-only registry
projections when no current row exists. With confirmed process absence but no
completion journal, it retains authenticated segment usage and marks cleanup
and total billing unconfirmed, without inventing a finish time. Such entries
report `no_result` when joined. A later sweep may update only recovery-owned
unconfirmed entries, and may not discard or alter previously confirmed usage.
Recovery does not signal old PIDs, resume execution, send announcements or
reprice usage. Existing monitor-owned rows remain with their original monitor.
This path remains under development; compilation is not recovery or
performance acceptance.

Decision-granted `spawn_batch` reserves member IDs and durably records a signed
membership plan before dispatch. A separate signed dispatch record distinguishes
started, rejected and unknown members. `join` reads this owner-scoped plan and
checks its parent decision; an incomplete dispatch record retains every planned
member, while a completed record excludes only explicit rejections. Missing run
results remain `no_result`. Dispatch errors are returned as
`dispatch_unconfirmed`, and a failure to persist dispatch confirmation retains
the batch ID in the response so callers can reconcile before retrying. These
records describe membership and dispatch evidence; they never authorize replay
of a child execution.

Process launch records also bind their batch ID. Before dispatch and during
recovery, PRX checks that the same owner and parent decision admit that run in
the signed batch plan. Recovered process entries retain the verified batch ID
for grouping; legacy launch records without a batch binding remain ungrouped.

When an authenticated completion has missing parent outcome projections,
recovery may use the service's already owned decision-store writer to persist
the exact authenticated outcome and parent link. Both writes require queue
acknowledgement, a completed drain and matching owner-scoped readback. Conflicts
remain unconfirmed. Recovery never opens or upgrades a writable store, creates
a key or repairs a database to obtain this capability; without an existing
writer, missing projections remain pending.

Issued child grants retain their configured logical source fabric. Nested
delegate and task-mode child provenance, segment admission and decision
settlement use that fabric, while execution-memory retrieval remains attached
to the tool's private memory. A restored worker grant accepts only a fabric
whose workspace matches its authenticated parent source. Worker session list,
history, status and lifecycle tools use this logical fabric; spawn keeps a
separate private execution-memory handle. Batch and historical-run lookup use
the logical workspace rather than the private worker directory. Decision-enabled
delegate and spawn require an issued child grant, and decision-enabled workers
require authenticated child authority. The candidate child path is implemented
but remains pending centralized behavior, security and performance validation.

Logical event recording remains attached when a nested turn has no private
execution-memory handle. For nested process workers, shared storage paths and
the parent backend identity come from the issued logical source; private worker
directories and private memory remain separate. Dispatch rejects a source
fabric backed by a different memory handle even if its workspace name matches.

Task and process children construct initial model history after child decision
admission and close handling. Granted process children enrich their recorded
raw input through the selected decision retrieval; they do not also run the
legacy shared-context loader. Ordinary workers retain that loader when building
their initial history. A close decision can return without preparing a model
prompt or performing this legacy context read.

Child request recording for delegate, task spawn and process workers retains a durable input receipt. A failed write, mismatched input/lineage or failed owner-scoped readback stops the child request instead of continuing with unverified provenance. Disabled user-message recording yields no durable receipt; its synthetic event is not source authority for enabled child decisions. Parent causation remains separate from the child request event.

Channel smart-group pre-gating runs after source decision admission. Clearly relevant messages enter the main loop directly; uncertain messages and heuristic noise require a resolver-built classifier call bound to the pinned configuration generation and the turn's physical accounting scope before they can produce a silent terminal. Classifier construction, accounting, timeout and parsing failures retain fail-open behavior. Bot-sender and cooldown protections remain pre-admission transport guards.

Channel decision source admission checks the recorded inbound event against its transport, owner, session, run, configuration generation and visibility, then reads back the complete owner-scoped event. Enabled decision mode requires this durable receipt. Raw routing input remains separate from the reserved-marker-neutralized history text. Channel runtime envelopes preserve the recorded private/group visibility. This source binding is part of the ongoing channel decision migration; it does not by itself establish full channel decision execution.

The channel candidate admits a source-bound decision before constructing any classifier or selected main provider. Direct close replies pass channel outbound authorization and identity-notice acknowledgement before terminal persistence and delivery. Running turns use decision-selected retrieval, constrained tool routing and reserved compaction. Completed, silent, failed and cancelled outcomes share a decision terminal guard; failed/cancelled turns retain known provider traces and usage uncertainty. Main-provider startup warmup, descendant cleanup acknowledgement and channel parity remain pending migration verification.

Channel model execution shares an outer descendant-cleanup scope. The parent terminal guard remains outside that scope and waits for its explicit acknowledgement, including on forced future drop. Context-overflow classification, compaction and provider replay are owned by the shared turn engine; channel and terminal-chat entrypoints do not compact or replay a completed engine call. A failed or missing cleanup acknowledgement leaves terminal settlement unconfirmed and suppresses failed result notices. Behavioral cancellation validation is still pending.

Signal vision preflight retains its own provider trace and source-owned physical accounting scope. Each supported physical request reserves token and PMM-priced monetary liability before sending, then settles fully reported usage. Missing or interrupted usage remains uncertain. The redacted auxiliary observation links that scope; its aggregate trace does not issue another usage settlement or cost debit. Main-loop snapshots do not overwrite auxiliary observations. Assembled failure, cancellation, replay and fixed-budget validation remain pending.

Auxiliary observations persist before descendant-cleanup acknowledgement when event recording is available; required durable receipts are checked by owner-scoped readback. Parent terminal persistence still waits for successful cleanup. Recording-disabled execution does not fabricate durable auxiliary replay evidence.

Channel prompts now advertise only skill metadata from the decision-selected capability set after execution-gate validation. This path does not run an independent per-turn Skill RAG query or load skill instruction bodies. Catalog snapshots retain pinned skill metadata and authenticated process transport validates its identity/binding digest. Agent/Chat/worker prompt migration and fixed-budget validation remain pending.

Decision-admitted Agent single-shot/interactive turns and Chat turns now use the same pinned skill metadata reader as channels. Remaining legacy/no-decision adapters and child/gateway prompt paths still require migration; this does not claim complete CDP-29 or prompt-parity acceptance.

Decision-admitted Gateway/Console turns now consume the same pinned skill metadata reader. Granted process-worker segments rebuild their leading system prompt from each segment decision, so steering cannot retain an earlier segment's skill list. Private execution-workspace prompt identity remains separate from the inherited catalog metadata paths. Legacy/no-decision paths and child execution parity remain subject to final migration and verification.

Issued synchronous delegates and task-mode child segments also append only their admitted skill metadata to the original delegated instructions. Steering replaces the system frame with a freshly selected view, preventing skill metadata from accumulating across segments.

Channel startup no longer constructs an unused static skill prompt, embedding-backed skill list, or Skill RAG context. The decision catalog owns metadata discovery, and admitted turns build their selected prompt view. Provider startup and excluded delivery-filter behavior are unchanged by this cleanup.

Console runtime execution now requires an admitted decision before prompt or context preparation; missing context fails explicitly. Its former independent skill/recall, unrestricted routing and unbound failure-finalization branches are removed. Gateway model-loop routing also refuses a missing decision rather than exposing the full registry. This remains part of the mandatory-context migration; the shared request type and other legacy adapters are not yet fully migrated.

Chat Redux runtime execution now rejects missing admission before compaction or provider execution. The driver uses only decision-owned routing and requires trace capture against that binding; its session-wide routing fallback is removed. Legacy unbound test fixtures still need genuine admission setup during centralized validation.

The ordinary Chat loop now requires source/provider binding and completes admission before context enrichment. Recall, skill metadata, compaction reservation and routing come from that decision, including disabled-plane built-in decisions. Removed independent per-turn recall/Skill RAG and whole-registry routing fallbacks; startup command/history skill metadata remains available without embedding initialization. Centralized parity and budget validation remain pending.

Agent CLI and interactive turns now consume admitted decision recall, skills, compaction and routing without independent per-turn fallbacks. Runtime envelope overrides require the existing Cron/Xin stored-creator admission before bootstrap; unsupported overrides cannot inherit local CLI authority. Startup command skill metadata remains available without embedding initialization. Legacy routing characterization fixtures are test-local and do not establish production admission parity.

Admitted Delegate directives now enter the shared execution path with exact selected capability references and their verified child manifest. The former placeholder hop to forced terminal selection is removed; closed contexts still reject execution. This connects existing issued-grant and child-ingress plumbing but does not establish child end-to-end acceptance. Ordinary ungranted/disabled children and explicit background-command admission remain separate migration work.

A selected Delegate is returned for execution only after its parent decision chain drains to storage and every owner-scoped replay digest matches the issued chain. Cancellation, dropped writes, missing rows and conflicting persisted decisions fail before child launch. Other directive paths retain their existing persistence policy. Process-launch race and cancellation tests remain pending.

Channel decisions participate in the shared terminal settlement and outcome-rejoin service. The admitted source, owner, generation, catalog and durable record checks remain required; this completes the service surface registration used by the channel terminal guard.

Delegation persistence and readback must finish within the original selected decision deadline; storage verification does not extend the configured latency budget.

Chat background launch, kill, steer, shell/PTY and decision-approval session commands require local terminal operator input. The same check applies during active turns and when plain text steers an attached session. Mailbox/model input cannot obtain operator authority through the shared command parser. Read-only session navigation remains available. This source check is separate from pending background-command decision admission and does not create a child grant.

The `/workers cancel` and `/workers stop` aliases also require local terminal operator input in both idle and active-turn handlers, before worker lookup or cancellation-state changes. Read-only `/workers` status remains available.

Ordinary configured delegates with the decision plane disabled now retain the exact PMM model/credential selection and actual provider instance through the child provider binding, including direct models and hints. This binding carries no parent grant and cannot issue a delegated source proof. Full ordinary child decision admission remains pending; legacy configuration-less isolated fixtures retain their existing construction path.

The ordinary child snapshot is derived only from its effective in-memory configuration, with canonical hashing and explicit workspace/config paths. It does not run startup configuration management, reload disk state, change proxy globals or refresh metric labels.

Ordinary delegate input events use the immutable child provider snapshot generation and revision rather than the caller's configuration labels. Before recording, the adapter verifies the actual provider instance, workspace, configured agent and delegate run/session/source identity. Existing durable input readback checks still apply; synthetic disabled-recording input does not become delegated authority.

The tool execution service passes a task-local typed caller context to delegate/sessions_spawn dispatch after execution checks. Delegate derives its actor and lineage directly from this context when present, independently of model JSON. The context is scoped to the backend future and does not cross spawned tasks automatically; ordinary child admission must explicitly bind and retain the verified source. Legacy direct-tool callers still need separate admission migration.

Within sessions_spawn execution, spawn/announcement lineage and run-owner confinement now prefer the execution service's typed caller context. Explicit owner isolation takes precedence over local-operator access, and a missing isolated owner admits no runs. Legacy direct-tool argument handling remains for callers outside this scope; it is not ordinary-child decision admission.

Ordinary delegates dispatched through the execution service inherit the typed caller's resolved owner, parent run/session, causation, attempt/lease and authority guard on input events. A workspace mismatch is refused. Their route record references the child input receipt when durable, or no source event when recording is disabled; it does not substitute the parent input or a placeholder owner. This lineage alignment does not yet complete ordinary decision admission.

Delegate tool projections retain the underlying read-only capability inventory and binding identities. An explicitly selected dynamic name is resolved from its exact live schema, including names outside a provider-facing listing window. A single-name projection exposes only that name; selecting a dispatcher root preserves its synthetic target bindings. This metadata alignment does not by itself complete ordinary child decision admission or establish delegated catalog parity.

For an issued child grant, delegation retains original backend roots so dynamic aliases and synthetic targets can be checked against the parent catalog. The grant is first checked against the named agent's configured `allowed_tools`; synthetic Skill/Node targets may use their configured dispatcher root, and the existing transcript lookup dependency remains eligible only when present in the grant. Per-call execution bindings still enforce the exact granted and selected capability references. Ordinary delegation without a grant retains its filtered tool projection while its decision admission is being completed.

Delegate input receipts bind the complete user input sent to the child, including the optional context block and task separators. Decision routing and the provider history use that same text; a granted child rejects a mismatched execution input before entering its tool loop.

### Workspace cost report

`prx cost` reads the existing workspace ledger without initializing configuration,
creating a database, migrating legacy state, or changing reservations. Use
`prx cost --json` for machine-readable output. Logging goes to stderr; the same
rule now applies to `prx evolution --json`.

The report shows settled ledger totals for today, the current month (both UTC),
and all time, plus an all-time breakdown by redacted model label. Request counts
separate provider-reported usage from estimated or legacy records. Outstanding
reservations are shown separately from settled charges, including dispatched
requests awaiting settlement even when their monetary reservation is zero.
Reservation amounts include estimates and are not vendor billing caps.

This is an advisory snapshot, not a billing export or a budget authorization.
Concurrent writes may require retrying it. Each ledger read is bounded to 8 MiB;
unsafe paths, incomplete or malformed rows, duplicate settlements and legacy
state requiring migration produce errors instead of an empty report. It uses
the existing Unix file-handle safety checks. There is no new persisted format;
rolling back removes the command without changing the ledger.

### Cost settlement replay

A terminal usage settlement is recorded once under its stable settlement ID.
Reusing that ID with a different persisted model, token count or charge returns
an error instead of silently reporting a successful replay. New cost rows also
retain a versioned digest of provider/model identity, cache token counts and
usage source; older rows are checked against the fields they actually contain.
A matching replay does not append another charge. Unknown pricing remains
explicit and is not converted to zero. This settlement check does not reserve
funds before a provider call; prospective budget admission remains a separate
requirement.

Cost ledger loading and refresh reject malformed nonblank rows, invalid amounts,
and overflowing current-period aggregates. Budget admission, recording, and
reporting return an error rather than silently undercounting the ledger. Preserve
the original ledger for diagnosis and restore verified records before retrying;
blank lines remain accepted.

Budget checks use integer micro-USD totals: each recorded charge and proposed
charge rounds upward, while daily and monthly limits round downward. Displayed
USD totals remain the recorded amounts; admission can therefore reject a request
whose unrounded display total appears below the limit. Ledger refresh and reopen
reconstruct the same per-record rounded totals. Invalid or out-of-range limits
refuse admission. If a known terminal charge has already occurred, it is persisted
before returning a budget-assessment error; an identical settlement replay does
not charge it twice. These checks are not atomic prospective reservations.

The local candidate now includes a bounded `state/costs.reservations.jsonl`
accounting journal. Its append operations share the cost-ledger process lock and
sync both the file and its directory. Outstanding reservations reduce mutable
budget checks and read-only remaining-budget snapshots, including across period
rollover. Each turn retains one allowance; each attempt retains its request and
binding digest. Dispatch consumes a reservation once, rechecks current cost
limits, and persists before the caller may send. Only an unsent reservation can
be explicitly released. Dropping a receipt does not release it; malformed or
incomplete journal data blocks admission without rewriting evidence.

The 2026-10-05 source addendum connects configured decision attempts and
supported auxiliary chat/embedding transports to versioned estimated
reservations. Builtin and cache-only decisions open no ledger. This does not
claim that every ordinary main-chat physical retry uses this auxiliary scope;
main terminal usage retains its existing aggregate settlement contract.
Reconciliation requires
an already persisted canonical cost row whose settlement ID is the physical
attempt ID. It verifies the exact replay fingerprint before recording the
terminal reservation state. The journal also binds a versioned digest of the
complete typed cost row, including model/token data, period timestamp, record ID
and session ID. Retaining an old settlement digest while changing those fields
cannot restore admission. This cross-file integrity check does not establish
transport price bounds or authenticate against an actor able to rewrite both
files. A crash between those writes retains both the actual charge and the
hold until reconciliation; matching replay does not
append another transition. Settled actual cost still consumes the turn allowance.
Recovery reconstructs accounting receipts without permitting another dispatch.
Request token sizing can overlap a prior physical attempt's background ledger
settlement. Preparation reuses only that request's sizing result and grants no
allowance; the next physical send still requires the prior durable settlement
and its own durable reservation within the original decision deadline.
Missing or conflicting cost projections block admission. Estimated reservations
record fully reported actual token counts and charges even when they exceed the
estimate; they do not classify ordinary underestimation as legacy bound
corruption. Actual debits reduce the remaining allowance and may prevent further
admission. Partial, missing or estimated usage retains the unresolved hold.
Legacy reservation records keep their original over-bound rejection semantics.

Fresh-process execution, cancellation/recovery, concurrency and fixed-budget
performance acceptance remain pending. Unsupported or unfunded calls remain
refused. Do not delete the journal to recover capacity or treat process exit,
timeout or cancellation as proof of zero charge.

### Embedded Agent model selection

For library callers using `AgentBuilder::runtime_config`, non-delegated
`task_routing` model hints take precedence over `query_classification` rules.
Selection uses each turn's original input. A same-provider route is resolved to
its concrete model before execution; an unmatched subsequent turn uses the
base model again. An initial hint must preserve the primary injected provider's
label and credential source. Per-turn cross-provider routes require an actual
instance registered with `AgentBuilder::route_provider`; selection must match
that instance's declared provider and credential source. Missing and duplicate
connection bindings are refused. A configured `Delegate` intent keeps the parent
connection unchanged and passes its child model hint only to the admitted
`sessions_spawn` process request. Pinned configuration can propose delegation
without inserting a tool name into the original input. This target is passed
internally to candidate generation: only the named delegator and applicable
clarification/refusal/approval outcomes are eligible. Other delegators, ordinary
tools and parent-model answers do not replace a configured delegation request.
All existing model, policy, scope and budget gates still apply. Execution requires
a selected `Delegate` directive and `sessions_spawn` capability, then uses the
shared tool execution service and child cleanup/terminal path. A closed decision
is honored; a different runnable action cannot silently invoke the parent model
instead. Successful launch returns its bounded run identity after parent terminal
commit; it does not claim the child task has completed. Full launch, model scope,
cancellation and allocation acceptance remain pending. Injected-router escalation
remains unsupported. See [embedded API admission](unified-turn-context.md#embedded-agent-api-admission)
for remaining compatibility limits; CLI and gateway routing contracts are separate.

Background chat approval waiters bind cleanup to an individual registration,
not just a run ID. Replacing a waiter closes the older request with deny; older
cancellation or completion cannot remove the replacement, restore its running
status or clear its approval banner. Delivering a decision consumes the sender
once, and a superseded resolver cannot return an earlier grant. Registration
sequence exhaustion refuses a new waiter. This in-process suspension safeguard
is separate from durable decision review, claim and one-use execution authority;
it does not extend approvals to another entry point.

Admitted Cron/Xin model turns now obtain a source-bound decision after recording their raw input. Enabled mode requires an exact durable owner-scoped event; disabled mode may use in-memory input without inventing an event ID. The proof pins task/run/configuration, actual provider/model selection and creator execution actor. Selected decisions drive recall, constrained execution and terminal settlement. Operator-created tasks retain the existing CLI execution tuple and semantic mirror eligibility; other tasks retain their creator-derived context and effective policy. Admission does not enable interactive clarification or an approval delivery channel.

## Skill lifecycle guide

See [Skill lifecycle](skill-lifecycle.md) for new instruction skills from
uncovered completed root tasks, frozen no-skill baselines, reviewed executable
imports, publication gates and recovery. Completed turns are not verified task
success, and local acceptance is not evidence of learning. Uncovered-task
recording stops at the 4096-experience threshold; this is not a hard cap on
managed-skill experiences. When the evolution scheduler and skill lifecycle
are enabled, each evolution tick removes at most 128 unreferenced terminal
experiences older than `runtime.retention.cold_days` in the evolution config.
Open tasks, task pins, parent-task lineage, and references from retained
patterns (including history) and proposals remain protected. Unreadable
evidence defers the sweep before deletion; incomplete tasks require terminal
recovery and are never aged out solely because they were idle.

Publication and rollback refresh the compatibility projection after commit;
`CommittedRepairRequired` leaves the binding live for recovery to repair.
Recovery resets the disabled marker to the binding's enabled state. First
publication refuses different unmanaged content. Linux and other non-macOS
platforms use native atomic directory exchange where supported and otherwise
a recoverable two-rename fallback. On macOS an existing projection is
exchanged in place; an unsupported exchange leaves that projection in place
and reports `CommittedRepairRequired`. See
[compatibility projection](skill-lifecycle.md#compatibility-projection).
Legacy executables are marked `ExecutableNeedsReview` and need operator
re-import to run managed. Managed native execution clears
the environment down to an allowlist and joins process cleanup; deliberate
`setsid` escape requires docker runtime containment today. See the
[upgrade actions](migration-unreleased.md) before adopting managed execution.

## Per-model levels and parameters

`[model_settings."<canonical catalog ID>"]` stores an optional `level`
(`low`, `medium`, `high`) and explicit declared parameters. Route entries can
set their own level and parameters, above the model settings. Durable changes
are validated and written through PMM transactions; invalid values are refused
before writing. See [model levels and selection commands](model-catalog.md#saving-a-model-level)
for examples, precedence, session-only chat choices and rollback.

## Release catalog lifecycle

`prx models catalog status` shows the embedded revision, digest and lifecycle
counts. Startup and doctor warn about deprecated, retired and absent model
references without rewriting stored selections. New retired selections are
refused. `prx models migrate-retired` previews published replacements;
`--yes` applies through PMM with normal validation, confirmation and history.
See [catalog lifecycle and migration](model-catalog.md#release-catalog-lifecycle)
for replacement conflicts, credential ownership and binary rollback.

The hosted decision latency upgrade warning applies to an inherited legacy
ceiling below the catalog recommendation. An explicit current-schema
`causal_tree.policy.extra_latency_budget_ms` is operator-owned and does not
emit that upgrade warning at load, status or doctor. Both paths preserve the
configured ceiling.

### Brave credential intake

Run `prx config brave-key --from-stdin` to inspect the current revision without
reading stdin. Then pipe the key to the same command with `--yes
--confirm-high-risk REVISION`. The confirmed transaction accepts at most 16 KiB,
validates the credential, and stores it through the existing secret encryption
policy. The key is never a CLI argument or part of plan output. A stale revision
is refused before stdin is consumed. Replacing or clearing it uses the same
managed configuration history and secret rules as other credentials.

### Managed script process containment

`[runtime] managed_cgroup_root = "/sys/fs/cgroup/prx-skills"` optionally selects
an operator-delegated Linux cgroup v2 subtree for managed native skill scripts.
PRX creates a unique child cgroup, joins it before exec, and writes `cgroup.kill`
on success, timeout, cancellation or abort. Normal completion waits up to five
seconds for the empty cgroup directory to be removed. Nested cgroups are disabled.
An unavailable root, missing kill interface, invalid filesystem or unsupported
platform refuses the script before execution. The setting is unset by default.

On macOS, or without this setting, cleanup uses process groups: a descendant
that calls `setsid` can escape. This is not a filesystem sandbox, nor protection
against a same-UID process explicitly migrating itself through a writable cgroup
hierarchy. Use OS/container access controls for hostile code. See the
[Linux cgroup v2 contract](https://docs.kernel.org/admin-guide/cgroup-v2.html).
The setting applies to lifecycle-managed native scripts; it does not claim to
change generic shell or Docker-container cleanup. Unset it to restore the
previous process-group behavior.

The Linux containment regression requires an empty, delegated cgroup v2 root:

```sh
PRX_TEST_CGROUP_ROOT=/sys/fs/cgroup/prx-test cargo test --lib setsid_descendant_is_killed_and_the_managed_group_is_removed -- --ignored
```

It starts a descendant in a new session, verifies its cgroup membership, kills the
managed group, joins the shell, and checks removal. This test is opt-in because an
ordinary developer shell does not necessarily own a delegated cgroup.

### Relative configuration directories and managed container cleanup

`--config-dir`, `OPENPRX_CONFIG_DIR` and `OPENPRX_WORKSPACE` are anchored to the
process working directory before subsystems derive paths. A relative directory
need not exist yet. Evolution analysis and other workspace paths therefore do
not append the configuration directory a second time.

Docker shell calls carry a fresh ownership label. After normal completion,
cancellation or timeout, PRX force-removes matching owned containers and checks
that none remain, within a ten-second cleanup deadline. A daemon failure or
unconfirmed removal is reported as a cleanup failure, never successful joined
cleanup. This supplements host process-group termination. Abrupt host/runtime
shutdown and an unreachable container daemon cannot provide synchronous cleanup
confirmation. Native macOS still has no cgroup boundary: use the Docker runtime
for managed scripts that can detach with `setsid`; a native process group alone
cannot contain them. Linux native containment requires the explicitly delegated
`managed_cgroup_root` described above.

Dropping an in-flight Docker call schedules bounded removal under the turn's
cleanup owner, so the turn cannot acknowledge cleanup before that task settles.
If work and terminal cleanup both fail, the error chain retains the original
work failure as well as the unconfirmed-cleanup marker.
