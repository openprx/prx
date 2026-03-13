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
# Max concurrent read-only tools in one batch (default: 2)
read_only_tool_concurrency_window = 2
# Per read-only tool timeout in seconds (default: 30)
read_only_tool_timeout_secs = 30

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
