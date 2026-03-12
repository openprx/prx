# OpenPRX

Self-hosted AI assistant framework built in Rust. Multi-channel, multi-provider, with built-in self-evolution.

Forked from [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) and extended with production reliability, governance-aware AI, and a self-evolution system.

## Highlights

- **14 LLM providers** — Anthropic, OpenAI, Google, DashScope, xAI, Ollama, and more
- **LLM Router** — heuristic routing (capability + Elo + cost + latency), KNN semantic routing (cold-start guard + 100ms timeout fallback), and Automix low-confidence auto-upgrade
- **19 messaging channels** — Signal, WhatsApp, Telegram, Discord, Slack, Matrix, and more
- **45+ built-in tools** — shell, browser, MCP, memory, scheduling, remote nodes
- **Web Console** — browser-based management interface (planned)
- **Remote Nodes** — control macOS/Linux/Pi devices via `prx-node` agent
- **Self-Evolution** — autonomous prompt/memory/strategy improvement (22 modules)
- **Subagent Governance** — concurrency limits, depth control, config inheritance

### LLM Router Flags

- `router.enabled` — enable heuristic model routing
- `router.knn_enabled` — enable semantic KNN scoring (with timeout-safe fallback)
- `router.automix.enabled` — enable cheap-first, low-confidence upgrade to premium model

## Quick Start

```bash
# Build
git clone https://github.com/openprx/prx.git && cd prx
cargo build --release --all-features

# Setup
cp target/release/openprx /usr/local/bin/
openprx onboard

# Run
openprx start
```

Default build (`cargo build`) includes `llm-router`.

Or download pre-built binaries from [Releases](https://github.com/openprx/prx/releases).

## Binaries

| Binary | Description |
|--------|-------------|
| `openprx` | Main AI daemon — providers, channels, tools, evolution |
| `prx-node` | Lightweight remote node agent — runs on managed devices |

## Architecture

```
         Channels (19)          Tools (45+)           Remote Nodes
    Signal · WA · TG · ...    Shell · MCP · ...     macOS · Pi · ...
              │                      │                     │
              ▼                      ▼                     ▼
         ┌─────────────────────────────────────────────────────┐
         │                    openprx daemon                    │
         │  Agent Loop · Gateway · Cron · Subagents · Memory   │
         └──────────────────────┬──────────────────────────────┘
                                │
                    Providers (14 LLMs)
              Anthropic · OpenAI · Google · ...
```

## Documentation

| Topic | Description |
|-------|-------------|
| [Providers](docs/providers.md) | 14 LLM providers, fallback chains, token refresh |
| [Channels](docs/channels.md) | 19 messaging platforms, DM/group policies |
| [Tools](docs/tools.md) | 45+ built-in tools, hooks system, webhooks |
| [Remote Nodes](docs/remote-nodes.md) | `prx-node` agent, device pairing, JSON-RPC |
| [Web Console](docs/web-console.md) | Browser-based management (planned) |
| [Evolution](docs/evolution.md) | Self-improvement pipeline |
| [Configuration](docs/configuration.md) | Config reference, workspace files, security |
| [Router](docs/router.md) | LLM Router config, flow, safety boundaries |
| [WASM Plugins](docs/plugin-developer-guide.md) | Plugin developer guide (Rust/Python/JS/Go) |
| [Host Function Reference](docs/host-function-reference.md) | WASM plugin host API reference |
| [Service Deploy/Restart SOP (QA)](docs/openprx-unified-service-deploy-restart-sop.md) | Unified systemd-only runtime, port conflict cleanup, plugin deploy/verify |

## Related Projects

| Repository | Description |
|------------|-------------|
| [openprx/prx](https://github.com/openprx/prx) | AI assistant framework (this repo) |
| [openprx/prx-memory](https://github.com/openprx/prx-memory) | Standalone memory MCP server |
| [openprx/openpr](https://github.com/openprx/openpr) | Project management platform |
| [openprx/openpr-webhook](https://github.com/openprx/openpr-webhook) | Webhook receiver for OpenPR |
| [openprx/wacli](https://github.com/openprx/wacli) | WhatsApp CLI with JSON-RPC daemon |

## Origin & License

Forked from [zeroclaw-labs/zeroclaw](https://github.com/zeroclaw-labs/zeroclaw) (MIT / Apache-2.0). "ZeroClaw" is a trademark of ZeroClaw Labs. This project is **OpenPRX**, an independent fork.

Dual-licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE).
