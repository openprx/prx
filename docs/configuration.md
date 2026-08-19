# Configuration

OpenPRX uses `~/.openprx/config.toml` as the main configuration file.

## Quick Setup

```bash
# Interactive setup wizard
prx onboard --interactive

# Inspect the effective configuration
prx config show
```

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

[runtime]
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

[heartbeat]
interval_minutes = 30
active_hours = [8, 23]

[agent]
# Read-only tool calls in one model iteration always run in parallel; there is
# no switch that turns that off. Tool execution is never time-bounded either —
# a long-running tool is normal agent business. Use `prx tasks list|kill` to see
# and stop work that is actually running away.
# Max concurrent read-only tools in one batch (default: 2)
read_only_tool_concurrency_window = 2
# Enable priority scheduling so foreground tools run before background batches.
priority_scheduling_enabled = false
# Optional list of low-priority/background tools.
# NOTE: priority is matched by tool name only (not by action), so `cron` is not
# listed here — adding it would demote every cron action, not just background runs.
low_priority_tools = ["sessions_spawn", "delegate"]

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
max_iterations = 200

# Model fallbacks
[reliability.model_fallbacks]
claude-opus-4-6 = ["claude-sonnet-4-6"]

# Provider fallbacks
fallback_providers = ["xai"]
```

An unrestricted profile is always explicit: set `workspace_only = false`,
clear `forbidden_paths`, and widen both ceilings deliberately. `prx doctor`
warns when all four unrestricted choices are active together.

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

## Shared Memory Fabric

PRX treats `chat`, `agent`, `gateway`, `channel`, `delegate`, and `sessions_spawn` as different message entrypoints over one workspace memory fabric.

- `message_events` stores normalized user, assistant, tool, and worker events.
- `memory_events` is the outbox/cursor stream used by SQLite polling watchers.
- `memories` stores promoted long-term semantic facts.
- `auto_save` no longer controls the base message log; it gates semantic promotion for backward compatibility.

`[memory.events]` controls raw/quasi-raw event recording. Turning it off stops new fabric event rows for entrypoints wired through `MemoryFabric`.

`[memory.semantic]` controls promotion from event/message content into durable semantic memory. Assistant promotion is disabled by default to reduce noisy self-generated memory.

For `sessions_spawn`, `task` runs an in-process sub-agent and `process` launches a worker process. Process mode uses a manifest with `memory_strategy`, `shared_memory_db_path`, and `worker_memory_db_path`; the default `shared_fabric` strategy writes worker events into the parent workspace fabric while still keeping the execution boundary explicit. Set `[sessions_spawn].process_memory_strategy = "isolated_private"` for a private worker DB. `hybrid` is fail-closed because no production merge consumer or merge/reject/ack/cleanup protocol exists; use `shared_fabric` or `isolated_private` instead.

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
convenient. Nothing rewrites your config files.

| Removed key | Why |
|---|---|
| `agent.parallel_tools` | no concurrency ceiling to switch |
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
| `sessions_spawn.max_spawn_depth` | nesting is uncapped; depth is still reported |
| `sessions_spawn.max_children_per_agent` | per-session fan-out is uncapped |
| `scheduler.max_concurrent` | due cron jobs all start at once; use `prx tasks` to see and end them |
| `autonomy.max_actions_per_hour` | no hourly action budget |
| `gateway.request_timeout_secs` | no request deadline on gateway handlers |
| `[security.resources]` (whole table) | never enforced by any code path |
| `memory.events.retention_days` | never read by the hygiene pass |

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

## WhatsApp Web Session Store

The Web-mode WhatsApp channel (`backend = "web"`) keeps its Signal-protocol
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
backend = "web"
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

- **Sandboxing**: Bubblewrap, Firejail, Landlock (Linux kernel), Docker
- **DM/Group policies**: Allowlist / open / disabled per channel
- **Context compaction**: Token-threshold trigger with full-chain propagation
- **Path validation**: Workspace-scoped file access with symlink protection
- **Memory ACL**: Per-user, per-project access control
- **Encrypted secret store**: For API keys, OAuth tokens

## LLM Router

OpenPRX includes an adaptive LLM Router with three switches:

- heuristic routing is always available (capability + Elo + cost + latency)
- semantic KNN routing activates when embeddings and enough history are available
- AutoMix activates when a premium model is configured

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
