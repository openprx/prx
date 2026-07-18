# Configuration

OpenPRX uses `~/.openprx/config.toml` as the main configuration file.

## Quick Setup

```bash
# Interactive setup wizard
openprx onboard

# Or quick non-interactive setup
openprx onboard --quick
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

[gateway]
request_timeout_secs = 180

[memory]
backend = "sqlite"
# Compatibility gate for semantic promotion. Message events are controlled below.
auto_save = true

[memory.events]
enabled = true
record_user_messages = true
record_assistant_messages = true
record_tool_events = false
retention_days = 14

[memory.semantic]
auto_promote_user_messages = true
auto_promote_assistant_messages = false
min_chars = 30

# Optional standalone external-event receiver. Durable topic, participant,
# memory, ingestion-state, and outbox writes use one SQLite transaction.
[webhook]
enabled = false
bind = "127.0.0.1:16899"
token = "replace-with-a-secret-token"
# When set, requests must also send X-Webhook-Signature: sha256=<HMAC hex>.
signing_secret = "replace-with-a-separate-hmac-secret"

[channels_config.signal]
enabled = true
account = "+1234567890"
dm_policy = "allowlist"
allowed_from = ["uuid:your-uuid"]

[channels_config.wacli]
enabled = true
host = "127.0.0.1"
port = 16867

[heartbeat]
enabled = true
interval_minutes = 30
active_hours = [8, 23]

[compaction]
enabled = true
compact_context = true

[agent]
# Master switch for parallel read-only scheduling (default: false).
parallel_tools = false
# Max concurrent read-only tools in one batch (default: 2)
read_only_tool_concurrency_window = 2
# Per read-only tool timeout in seconds (default: 30)
read_only_tool_timeout_secs = 30
# Enable priority scheduling so foreground tools run before background batches.
priority_scheduling_enabled = false
# Optional list of low-priority/background tools.
# NOTE: priority is matched by tool name only (not by action), so `cron` is not
# listed here — adding it would demote every cron action, not just background runs.
low_priority_tools = ["sessions_spawn", "delegate"]
# Rollout stage: off | stage_a | stage_b | stage_c | full
concurrency_rollout_stage = "off"
# Optional sample percent (0 means stage default)
concurrency_rollout_sample_percent = 0
# Optional channel allowlist for rollout
concurrency_rollout_channels = ["telegram", "discord"]
# Emergency kill switch (highest priority) to force serial scheduling
concurrency_kill_switch_force_serial = false
# Auto rollback thresholds
concurrency_auto_rollback_enabled = true
concurrency_rollback_timeout_rate_threshold = 0.20
concurrency_rollback_cancel_rate_threshold = 0.20
concurrency_rollback_error_rate_threshold = 0.20

[subagent_governance]
max_concurrent_subagents = 4
max_spawn_depth = 2
max_children_per_agent = 5

# Secure autonomous defaults. `full` skips confirmation prompts but remains
# workspace-scoped and bounded unless the operator explicitly widens it.
[autonomy]
level = "full"
workspace_only = true
forbidden_paths = ["/etc", "/root", "/home", "/opt", "/tmp", "~/.ssh"]
max_actions_per_hour = 20
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
tasks; Xin is the only execution scheduler even when `[xin].enabled` is false.
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

## Agent Concurrency Env Overrides

- `ZEROCLAW_READ_ONLY_TOOL_CONCURRENCY_WINDOW`
- `ZEROCLAW_READ_ONLY_TOOL_TIMEOUT_SECS`
- `ZEROCLAW_PRIORITY_SCHEDULING_ENABLED`
- `ZEROCLAW_CONCURRENCY_KILL_SWITCH_FORCE_SERIAL`
- `ZEROCLAW_CONCURRENCY_ROLLOUT_STAGE`
- `ZEROCLAW_CONCURRENCY_ROLLOUT_SAMPLE_PERCENT`
- `ZEROCLAW_CONCURRENCY_ROLLOUT_CHANNELS` (comma-separated)
- `ZEROCLAW_CONCURRENCY_AUTO_ROLLBACK_ENABLED`
- `ZEROCLAW_CONCURRENCY_ROLLBACK_TIMEOUT_RATE_THRESHOLD`
- `ZEROCLAW_CONCURRENCY_ROLLBACK_CANCEL_RATE_THRESHOLD`
- `ZEROCLAW_CONCURRENCY_ROLLBACK_ERROR_RATE_THRESHOLD`

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

## Security

- **Sandboxing**: Bubblewrap, Firejail, Landlock (Linux kernel), Docker
- **DM/Group policies**: Allowlist / open / disabled per channel
- **Context compaction**: Token-threshold trigger with full-chain propagation
- **Path validation**: Workspace-scoped file access with symlink protection
- **Memory ACL**: Per-user, per-project access control
- **Encrypted secret store**: For API keys, OAuth tokens

## LLM Router

OpenPRX includes an adaptive LLM Router with three switches:

- `router.enabled` — heuristic routing (capability + Elo + cost + latency)
- `router.knn_enabled` — semantic KNN routing (cold-start guard + timeout fallback)
- `router.automix.enabled` — low-confidence auto-upgrade to premium model

Minimum router config (single reachable provider):

```toml
[general]
default_provider = "openrouter"
default_model = "openai/gpt-4o-mini"

[router]
enabled = true
knn_enabled = false

[router.automix]
enabled = false

[[router.models]]
model_id = "gpt-4o-mini"
provider = "openrouter"
categories = ["conversation"]
```

For full examples, field-by-field reference, flow, and security boundaries, see [docs/router.md](router.md).
