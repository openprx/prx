# Changelog

All notable changes to OpenPRX will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-03-19

### Added

- **Xin (心) autonomous task engine** — Configuration-driven heartbeat
  scheduler for system-level autonomous work.
  - 5 built-in system tasks: health check, stale cleanup, memory
    evolution, fitness report, memory hygiene
  - 3 execution modes: Internal (Rust fn), AgentSession (LLM),
    Shell (command)
  - SQLite-backed task persistence with execution history
    (`xin/tasks.db`)
  - LLM tool (`xin`) with 7 actions: list, add, get, remove, status,
    pause, resume
  - Configurable: interval, max_concurrent, max_tasks, stale_timeout,
    builtin_tasks
  - Evolution/fitness integration mode — xin can take over standalone
    schedulers
  - Supervisor with exponential backoff restart and health monitoring
- **Chat module** — Extracted conversational session management with
  named constants
- **Terminal channel** — Dedicated terminal-based messaging channel
- `SecretStore::decrypt_and_migrate()` — Auto-migrate legacy `enc:`
  to `enc2:` (ChaCha20-Poly1305 AEAD)
- `SecretStore::needs_migration()` / `is_secure_encrypted()` — Secret
  format detection
- **Telegram mention_only mode** — Bot only responds to @-mentions
  in group chats

### Security

- **26-finding comprehensive audit** — Full regression audit of 170K+
  LOC, all findings fixed:
  - (C-1) SQLite foreign keys enabled in memory backend
  - (C-2) Cron atomic job claiming prevents double-execution
  - (C-3) SSRF DNS rebinding defense with resolved IP validation
  - (H-1..H-8) Memory LRU eviction, content hash expansion (128-bit),
    tool argument schema validation, MCP debug log redaction, rate
    limiting for web_fetch/http_request
  - (M-1..M-11) Optimistic concurrency for xin/cron stores, magic
    number constants, flaky test serialization
  - (L-1..L-4) Code quality improvements
- **Web console hardening** — 9 additional fixes:
  - (C-1) Rate limiter time arithmetic safety
  - (C-2) Config dual-store atomic update (Mutex + ArcSwap)
  - (C-3) Upload path traversal defense — reject absolute paths
    and `..` components
  - (H-1) Auth middleware now supports cookie authentication with
    CSRF protection
  - (H-2) Skill install URL validation — strict host parsing
    prevents prefix bypass
  - (H-3) WebSocket log stream connection limit (max 64 concurrent)
  - (M-1) Extended sensitive key detection patterns
  - (M-3) Pagination clamp allows small page sizes
  - (L-1) API error responses no longer leak internal Rust error
    details
- **Legacy XOR cipher migration**: `enc:` prefix deprecated,
  auto-migrated to `enc2:`

### Fixed

- **Flaky proxy cache test** — Added `Mutex` serialization to prevent
  global cache race condition
- **Onboarding channel menu** — Enum-backed selector instead of
  hard-coded numeric match arms
- **OpenAI native tool spec** — Owned serializable structs for tool
  schema validation
- **Router audit fixes** — Provider reachability filtering, lock-safe
  async persistence, reserved `router/` namespace

### Deprecated

- `enc:` prefix for encrypted secrets — Use `enc2:`
  (ChaCha20-Poly1305) instead

## [0.2.1] - 2026-03-11

### Added

- **LLM Router Phase 1-5** — Delivered heuristic routing, capability
  registry, feedback loop updates, KNN semantic routing, and Automix
  adaptive escalation.

### Fixed

- **Router audit fixes** — Applied critical/high audit hardening for
  provider reachability filtering, lock-safe async outcome persistence,
  and reserved `router/` namespace enforcement.

## [0.1.0] - 2026-02-13

### Added

- **Core Architecture**: Trait-based pluggable system for Provider,
  Channel, Observer, RuntimeAdapter, Tool
- **Provider**: OpenRouter implementation (access Claude, GPT-4,
  Llama, Gemini via single API)
- **Channels**: CLI channel with interactive and single-message modes
- **Observability**: NoopObserver (zero overhead), LogObserver
  (tracing), MultiObserver (fan-out)
- **Security**: Workspace sandboxing, command allowlisting, path
  traversal blocking, autonomy levels (ReadOnly/Supervised/Full),
  rate limiting
- **Tools**: Shell (sandboxed), FileRead (path-checked), FileWrite
  (path-checked)
- **Memory (Brain)**: SQLite persistent backend (searchable, survives
  restarts), Markdown backend (plain files, human-readable)
- **Heartbeat Engine**: Periodic task execution from HEARTBEAT.md
- **Runtime**: Native adapter for Mac/Linux/Raspberry Pi
- **Config**: TOML-based configuration with sensible defaults
- **Onboarding**: Interactive CLI wizard with workspace scaffolding
- **CLI Commands**: agent, gateway, status, cron, channel, tools,
  onboard
- **CI/CD**: GitHub Actions with cross-platform builds (Linux, macOS
  Intel/ARM, Windows)
- **Tests**: 159 inline tests covering all modules and edge cases
- **Binary**: 3.1MB optimized release build (includes bundled SQLite)

### Security

- Path traversal attack prevention
- Command injection blocking
- Workspace escape prevention
- Forbidden system path protection (`/etc`, `/root`, `~/.ssh`)

[Unreleased]: https://github.com/openprx/prx/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/openprx/prx/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/openprx/prx/compare/v0.1.0...v0.2.1
[0.1.0]: https://github.com/openprx/prx/releases/tag/v0.1.0
