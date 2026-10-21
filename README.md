# OpenPRX

Self-hosted AI assistant framework built in Rust. Multi-channel, multi-provider, with built-in self-evolution.

Forked from [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) and extended with production reliability, governance-aware AI, and a self-evolution system.

## Highlights

- **Broad LLM provider catalog** — Anthropic, OpenAI, Google Gemini, GitHub Copilot, Ollama, AWS Bedrock, GLM, OpenAI Codex, local runtimes, and compatible endpoints
- **LLM Router** — heuristic routing (capability + Elo + cost + latency), KNN semantic routing (cold-start guard + 100ms timeout fallback), and Automix low-confidence auto-upgrade
- **Causal Tree Engine** — admitted decisions with rehearsal, scoring, and circuit breaker; enabled by default with the built-in backend
- **Multi-channel messaging** — Signal, WhatsApp, Telegram, Discord, Slack, opt-in Matrix, and more
- **Built-in tools and integrations** — shell, MCP, memory, scheduling, remote nodes, and an integration catalog
- **Xin (心) task engine** — autonomous heartbeat scheduler with 3 execution modes (Rust/LLM/Shell), 3 built-in maintenance tasks, durable Goal/Step execution, and SQLite persistence
- **Web Console** — browser-based management interface (`console/`)
- **Remote Nodes** — control macOS/Linux/Pi devices via `prx-node` agent
- **Self-Evolution** — autonomous prompt/memory/strategy improvement with dedicated self-system scheduling
- **Subagent Spawning** — ordinary fan-out without a global concurrency cap, config inheritance, and explicit bounded recursive grants; live runs listed and ended with `prx tasks`
- **Extensive automated test suite** — unit, integration, PTY, gateway, migration, and security coverage

### LLM Router Flags

- heuristic model routing is always available
- semantic KNN scoring activates when embeddings and history are available
- cheap-first, low-confidence upgrade activates when a premium model is configured

### Causal Tree Engine Flags

> **Enabled by default.** `auto` serves the built-in backend without a remote
> endpoint. Remote processing requires explicit configuration and opt-in.

- `causal_tree.enabled` — master switch (default: `true`); explicit versioned `false` stays disabled
- `causal_tree.schema_version` — stored configuration schema (current: `2`)
- `causal_tree.policy.max_candidates` — maximum candidates (default: `12`)
- `causal_tree.policy.commit_threshold` — minimum commit score (default: `0.62`)
- `causal_tree.policy.extra_latency_budget_ms` — additional decision budget (built-in default: `500` ms; hosted backend selection can adopt its catalog recommendation)
- `causal_tree.rehearsal.timeout_ms` — rehearsal timeout (default: `100` ms)
- `causal_tree.backend.circuit_breaker.failure_threshold` — consecutive failures (default: `5`)
- `causal_tree.scoring.weights` — optional scorer-owned weight overrides

See [decision-plane behavior](docs/decision-plane.md) and the
[configuration reference](docs/configuration.md) for admission, defaults and
legacy-key migration.

## Quick Start

Linux and macOS:

```bash
curl -fsSL https://github.com/openprx/prx/releases/latest/download/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
prx onboard --interactive
prx daemon
```

Windows PowerShell:

```powershell
irm https://github.com/openprx/prx/releases/latest/download/install.ps1 | iex
prx onboard --interactive
prx daemon
```

The installer downloads a platform-specific binary from
[GitHub Releases](https://github.com/openprx/prx/releases), verifies its SHA-256
checksum, and installs it without requiring Rust or Git. See the
[installation guide](docs/one-click-bootstrap.md) for exact versions, source
builds, supported platforms, upgrades, and removal.

To build from a checkout with the pinned Rust 1.99.0 toolchain:

```bash
git clone https://github.com/openprx/prx.git
cd prx
cargo build --release --locked --bin prx
./target/release/prx onboard --interactive
```

Release binaries and normal source builds leave Matrix support out. To build
the Matrix channel, opt in explicitly:

```bash
cargo build --release --locked --bin prx --features channel-matrix
```

## Binaries

| Binary | Description |
|--------|-------------|
| `prx` | Main AI daemon and CLI — providers, channels, tools, evolution |
| `prx-node` | Lightweight remote node agent — runs on managed devices |

## Architecture

```
             Channels             Tools              Remote Nodes
    Signal · WA · TG · ...    Shell · MCP · ...     macOS · Pi · ...
              │                      │                     │
              ▼                      ▼                     ▼
         ┌─────────────────────────────────────────────────────┐
         │                       prx daemon                     │
         │  Agent Loop · Gateway · CTE · Xin · Memory · Evo   │
         └──────────────────────┬──────────────────────────────┘
                                │
                          Providers
              Anthropic · OpenAI · Google · ...
```

## Documentation

| Topic | Description |
|-------|-------------|
| [Installation](docs/one-click-bootstrap.md) | Binary install, source build, upgrades, rollback |
| [Providers](docs/providers.md) | Provider catalog, fallback chains, token refresh |
| [Channels](docs/channels.md) | Messaging platforms, DM/group policies |
| [Tools](docs/tools.md) | Built-in tools, hooks system, webhooks |
| [Remote Nodes](docs/remote-nodes.md) | `prx-node` agent, device pairing, JSON-RPC |
| [Web Console](docs/web-console.md) | Browser-based management interface |
| [Skill Lifecycle](docs/skill-lifecycle.md) | Instruction revisions, evidence, evaluation gates, publication and rollback |
| [Evolution](docs/evolution.md) | Self-improvement pipeline |
| [Configuration](docs/configuration.md) | Config reference, workspace files, security |
| [Router](docs/router.md) | LLM Router config, flow, safety boundaries |
| [WASM Plugins](docs/plugin-developer-guide.md) | Plugin developer guide (Rust/Python/JS/Go) |
| [Host Function Reference](docs/host-function-reference.md) | WASM plugin host API reference |
| [Plugin Runtime Lifecycle](docs/plugin-runtime-lifecycle.md) | Atomic generations, subscriber pumps, trust and hook bounds |
| [Release Gate](docs/release-gate.md) | Exact-SHA CI/security gate that guards release publication |

## Links

- [Documentation](https://docs.openprx.dev/en/prx/) — Full PRX documentation
- [Community](https://community.openprx.dev) — OpenPRX community forum
- [OpenPRX](https://openprx.dev) — Project homepage

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
