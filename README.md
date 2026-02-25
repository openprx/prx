# OpenPRX

AI assistant framework built in Rust. Self-hosted, multi-channel, multi-provider, with built-in self-evolution.

Forked from [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) and extended with governance-aware AI capabilities, production reliability hardening, and a self-evolution system.

## Origin & License

OpenPRX is a derivative work of ZeroClaw, originally created by ZeroClaw Labs under the MIT + Apache-2.0 dual license. We gratefully acknowledge the upstream project and its contributors.

- **Upstream**: [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw) (MIT / Apache-2.0)
- **This fork**: [openprx/prx](https://github.com/openprx/prx) — same dual license
- **"ZeroClaw"** is a trademark of ZeroClaw Labs. This project is **OpenPRX**, an independent fork.

See [LICENSE](LICENSE) (MIT) and [LICENSE-APACHE](LICENSE-APACHE) for full terms.

## What Changed from ZeroClaw

| Area | ZeroClaw | OpenPRX |
|------|----------|---------|
| Name | `zeroclaw` binary | `openprx` binary (`zeroclaw` symlink for compat) |
| Config | `~/.zeroclaw/` | `~/.openprx/` (fallback `~/.zeroclaw/`) |
| Env vars | `ZEROCLAW_*` | `OPENPRX_*` (fallback `ZEROCLAW_*`) |
| Providers | 10 providers | **14 providers** (+LiteLLM, vLLM, HuggingFace, GLM) |
| Channels | 18 channels | **19 channels** (+wacli for WhatsApp CLI) |
| Evolution | — | **Self-evolution system** (22 modules, ~9500 lines) |
| Subagents | Basic spawn | **Governed** (concurrency, depth, config inheritance) |
| Security | Basic | **3-phase hardened** (DM/group policy, compaction, timeouts) |
| Anthropic | API key only | **OAuth auto-refresh** from Claude CLI credentials |
| MCP | Client | Client + **OpenPR MCP integration** |

## Features

### Multi-Provider (14 providers)

| Provider | Models | Notes |
|----------|--------|-------|
| Anthropic | Claude Opus, Sonnet, Haiku | OAuth auto-refresh support |
| OpenAI | GPT-4o, GPT-5, o1/o3 | Codex models via dedicated provider |
| Google | Gemini 2.x | |
| Ollama | Any local model | |
| OpenRouter | 100+ models | |
| AWS Bedrock | Claude, Titan, etc. | |
| GitHub Copilot | GPT-4o | Token auto-refresh |
| GLM (Zhipu) | GLM-4, GLM-5 | Chinese AI models |
| LiteLLM | Unified proxy | Route to 100+ providers |
| vLLM | Self-hosted | High-throughput inference |
| HuggingFace | Open models | Inference API |
| Compatible | Any OpenAI-compatible | Custom base URL |

Provider features: automatic fallback chains, model routing, token refresh, rate limiting.

### Multi-Channel (19 channels)

Signal · WhatsApp (whatsmeow) · WhatsApp CLI (wacli) · Telegram · Discord · Slack · iMessage · Matrix · IRC · Email · DingTalk · Lark/Feishu · QQ · Mattermost · Nextcloud Talk · LinQ · CLI

### 45+ Built-in Tools

| Category | Tools |
|----------|-------|
| **Shell & Files** | `shell`, `file_read`, `file_write`, `git_operations` |
| **Web** | `web_search`, `web_fetch`, `http_request` |
| **Browser** | `browser` (automation), `browser_open`, `screenshot`, `canvas` |
| **Memory** | `memory_store`, `memory_recall`, `memory_search`, `memory_get`, `memory_forget` |
| **Messaging** | `message_send`, `tts` (text-to-speech) |
| **Sessions** | `sessions_spawn`, `sessions_send`, `sessions_list`, `sessions_history`, `session_status`, `subagents`, `delegate` |
| **Scheduling** | `cron_add`, `cron_list`, `cron_update`, `cron_remove`, `cron_run`, `cron_runs`, `schedule` |
| **Images** | `image`, `image_info` |
| **MCP** | `mcp` (Model Context Protocol client — connect to any MCP server) |
| **Remote Nodes** | `nodes` (control paired devices — camera, screen, location, run commands) |
| **Infrastructure** | `gateway`, `config_reload`, `proxy_config`, `agents_list` |
| **Integrations** | `composio` (1000+ OAuth apps), `pushover` (notifications) |
| **Hardware** | `hardware_board_info`, `hardware_memory_map`, `hardware_memory_read` (ESP32/Arduino) |

### Hooks System

Event-driven hooks for extending agent behavior without modifying core code:

- **Hook events**: `agent_start`, `agent_end`, `llm_request`, `llm_response`, `tool_call_start`, `tool_call_end`, `message_received`, `message_sent`
- **Configuration**: `hooks.json` in workspace — map events to shell commands
- **Use cases**: Logging, metrics, external notifications, custom workflows
- **Timeout protection**: Configurable per-hook timeout (default 5s)

### Webhook Receiver

Built-in HTTP webhook endpoint for receiving external events:

- HMAC-SHA256 signature verification
- Memory-backed event storage
- Route external events (GitHub, CI/CD, monitoring) into agent context

### Remote Nodes

Pair and control remote devices over HTTP/2:

- **Camera**: Snap photos (front/back), record clips
- **Screen**: Screen recording and capture
- **Location**: GPS coordinates
- **Run**: Execute commands on paired devices
- **Notify**: Push notifications to devices
- **Transport**: HTTP/2 with TLS, pairing-based authentication

Includes `zeroclaw-node` binary for running on remote devices (Raspberry Pi, phones, etc.).

### Self-Evolution System

Autonomous improvement without LLM weight training — evolves prompts, memory, and strategies based on interaction data.

```
Record (realtime) → Analyze (daily) → Evolve (every 3 days)
```

- **Record layer**: Trace every interaction, tool call, and outcome
- **Memory system**: Retrieval, safety filtering, compression, anti-pattern detection
- **Analysis**: Automated evaluation with judge model and test suites
- **Evolution engines**: Memory evolution, prompt evolution, strategy evolution
- **Safety**: Rollback capability, gate checks, shadow mode for first rounds
- **Pipeline**: Scheduler, pipeline orchestration, annotation system
- **22 modules, ~9500 lines** of evolution infrastructure

### Subagent Governance

- **Spawn**: `sessions_spawn` with isolated or persistent sessions
- **Delegate**: `delegate` for inline sub-agent execution
- Max concurrent subagents (default: 4)
- Max spawn depth (default: 2) — propagated across processes
- Max children per agent (default: 5)
- Config inheritance: provider, model, API key, iterations, compaction
- Isolated sessions with configurable timeouts

### Memory System

Multiple storage backends with unified API:

| Backend | Description |
|---------|-------------|
| **SQLite** | Default, local, FTS5 full-text search + vector search |
| **Lucid** | Lightweight markdown-based memory |
| **PostgreSQL** | Scalable, multi-user |
| **Markdown** | File-based, human-readable |

Features: memory ACL (per-user/per-project access control), topic system, embedding cache, chunking, snapshot/restore, hygiene (auto-cleanup).

### Security

- **Sandboxing**: Bubblewrap, Firejail, Landlock (Linux kernel), Docker — pluggable backends
- **DM policy**: Allowlist / open / disabled per channel
- **Group policy**: Allowlist / open with group-level filtering
- **Context compaction**: Token-threshold trigger, full-chain propagation
- **Gateway timeout**: Configurable (default 60s, recommended 180s for complex tasks)
- **Path validation**: Workspace-scoped file access with symlink protection
- **Memory ACL**: Per-user, per-project access control with audit logging
- **Encrypted secret store**: For API keys, OAuth tokens
- **Pairing authentication**: For remote node connections

### Cron & Scheduling

- Cron expressions, intervals, or one-shot timers
- Per-job session isolation (main session or isolated)
- `systemEvent` (inject into session) or `agentTurn` (run agent)
- Delivery: announce to chat, webhook POST, or silent

### Heartbeat

- Configurable active hours (respect quiet time)
- Custom heartbeat prompt
- Background task scheduling via cron
- Proactive checks (email, calendar, weather)

### Observability

- **OpenTelemetry**: Traces and metrics export
- **Prometheus**: Metrics endpoint
- **Structured logging**: Tracing with env-filter
- **Verbose mode**: Detailed tool/LLM interaction logging

### Hardware Support

ESP32 and Arduino firmware for physical agent nodes:

- `firmware/zeroclaw-esp32` — Rust firmware for ESP32
- `firmware/zeroclaw-esp32-ui` — UI firmware
- `firmware/zeroclaw-uno-q-bridge` — Arduino Uno bridge

## Quick Start

### Prerequisites

- Rust 1.75+
- One LLM provider API key (Anthropic, OpenAI, Ollama, etc.)

### Install

```bash
git clone https://github.com/openprx/prx.git
cd prx
cargo build --release

# Install binary
cp target/release/openprx /usr/local/bin/
ln -s /usr/local/bin/openprx /usr/local/bin/zeroclaw  # backward compat
```

### Setup

```bash
# Interactive setup wizard
openprx onboard

# Or quick non-interactive setup
openprx onboard --quick
```

This creates `~/.openprx/config.toml` with your provider, channel, and identity configuration.

### Run

```bash
# Start the daemon
openprx start

# Or run as systemd service
cp openprx.service ~/.config/systemd/user/
systemctl --user enable --now openprx
```

### CLI

```bash
openprx start          # Start daemon
openprx stop           # Stop daemon
openprx status         # Show status
openprx doctor         # Diagnose issues
openprx config show    # Show current config
openprx config edit    # Edit config
openprx onboard        # Setup wizard

# Evolution (when enabled)
openprx evolution status    # Show evolution state
openprx evolution trigger   # Manually trigger evolution cycle
openprx evolution rollback  # Rollback last evolution
```

## Configuration

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
port = 8687

[heartbeat]
enabled = true
interval_minutes = 30
active_hours = "08:00-23:00"

[compaction]
enabled = true
compact_context = true

[subagent_governance]
max_concurrent_subagents = 4
max_spawn_depth = 2
max_children_per_agent = 5

# Multi-agent setup
[agents.researcher]
provider = "anthropic"
model = "claude-sonnet-4-6"
max_iterations = 200
```

## Architecture

```
                    ┌──────────────┐
                    │   Gateway    │
                    │  (HTTP API)  │
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
        ┌─────▼─────┐ ┌───▼───┐ ┌─────▼─────┐
        │  Channels  │ │ Agent │ │   Tools   │
        │ Signal,WA  │ │ Loop  │ │ 45+ tools │
        │ TG,Discord │ │       │ │           │
        └────────────┘ └───┬───┘ └───────────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
        ┌─────▼─────┐ ┌───▼───┐ ┌─────▼─────┐
        │ Providers  │ │Memory │ │ Evolution │
        │ 14 LLMs    │ │SQLite │ │ 22 modules│
        │            │ │Lucid  │ │ ~9500 LOC │
        └────────────┘ └───────┘ └───────────┘
```

| Component | Description |
|-----------|-------------|
| **Gateway** | HTTP API for external control (cron, wake, config) |
| **Channels** | 19 messaging platform integrations |
| **Agent Loop** | Message processing, LLM interaction, tool execution |
| **Tools** | 45+ built-in tools (shell, file, web, browser, MCP, etc.) |
| **Providers** | 14 LLM provider integrations with fallback chains |
| **Memory** | SQLite/Lucid/Postgres/Markdown memory backends |
| **Evolution** | Self-improvement system (record → analyze → evolve) |

## Workspace Files

OpenPRX uses workspace files for agent identity and memory:

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

These files are automatically injected into the agent context at startup (zero tool calls).

## OpenPR Integration

OpenPRX integrates with [OpenPR](https://github.com/openprx/openpr) (project management platform) via MCP:

- Query and create issues, proposals, comments
- Participate in governance votes as an AI agent
- Track sprint progress and project status
- Receive webhook notifications via [openpr-webhook](https://github.com/openprx/openpr-webhook)

## Roadmap

- [ ] Web UI for configuration and monitoring
- [ ] Plugin system for custom tools
- [ ] Multi-agent collaboration protocols
- [ ] Voice channel support (bidirectional)
- [ ] Evolution dashboard and analytics
- [ ] Distributed deployment (multi-node)

## Related Projects

| Repository | Description |
|------------|-------------|
| [openprx/prx](https://github.com/openprx/prx) | AI assistant framework (this repo) |
| [openprx/openpr](https://github.com/openprx/openpr) | Project management platform |
| [openprx/openpr-webhook](https://github.com/openprx/openpr-webhook) | Webhook receiver |
| [openprx/wacli](https://github.com/openprx/wacli) | WhatsApp CLI with JSON-RPC daemon |
| [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw) | Upstream project |

## Contributing

Contributions welcome. Please open an issue first to discuss changes.

## License

Dual-licensed under MIT and Apache-2.0. You may use either license.

- [MIT License](LICENSE)
- [Apache License 2.0](LICENSE-APACHE)

Original work copyright ZeroClaw Labs. Modifications copyright OpenPRX contributors.
