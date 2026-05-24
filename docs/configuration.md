# Configuration

OpenPRX uses `~/.openprx/config.toml` as the main configuration file.

## Quick Setup

```bash
# Interactive setup wizard
openprx onboard

# Or quick non-interactive setup
openprx onboard --quick
```

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
active_hours = "08:00-23:00"

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
low_priority_tools = ["sessions_spawn", "delegate", "cron_run"]
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

# Multi-agent setup
[agents.researcher]
provider = "anthropic"
model = "claude-sonnet-4-6"
max_iterations = 200

# Model fallbacks
[reliability.model_fallbacks]
claude-opus-4-6 = ["claude-sonnet-4-6"]

# Provider fallbacks
fallback_providers = ["xai"]
```

## Shared Memory Fabric

PRX treats `chat`, `agent`, `gateway`, `channel`, `delegate`, and `sessions_spawn` as different message entrypoints over one workspace memory fabric.

- `message_events` stores normalized user, assistant, tool, and worker events.
- `memory_events` is the outbox/cursor stream used by SQLite polling watchers.
- `memories` stores promoted long-term semantic facts.
- `auto_save` no longer controls the base message log; it gates semantic promotion for backward compatibility.

`[memory.events]` controls raw/quasi-raw event recording. Turning it off stops new fabric event rows for entrypoints wired through `MemoryFabric`.

`[memory.semantic]` controls promotion from event/message content into durable semantic memory. Assistant promotion is disabled by default to reduce noisy self-generated memory.

For `sessions_spawn`, `task` runs an in-process sub-agent and `process` launches a worker process. Process mode uses a manifest with `memory_strategy`, `shared_memory_db_path`, and `worker_memory_db_path`; the default `shared_fabric` strategy writes worker events into the parent workspace fabric while still keeping the execution boundary explicit. Set `[sessions_spawn].process_memory_strategy = "isolated_private"` for a private worker DB, or `"hybrid"` for private worker draft state with parent-recorded spawn/result events.

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
