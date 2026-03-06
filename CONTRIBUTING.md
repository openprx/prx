# Contributing to OpenPRX

Thanks for your interest in contributing!

## Development Setup

```bash
git clone https://github.com/openprx/prx.git
cd prx

# Build
cargo build

# Run tests
cargo test --locked

# Format & lint
cargo fmt --check
cargo clippy -- -D warnings

# Release build
cargo build --release --locked
```

## Architecture

Trait-based pluggable architecture — every subsystem is swappable:

```
src/
├── providers/       # LLM backends     → Provider trait
├── channels/        # Messaging         → Channel trait
├── tools/           # Agent tools       → Tool trait
├── memory/          # Persistence       → Memory trait
├── nodes/           # Remote devices    → Node server/client
├── observability/   # Metrics/logging   → Observer trait
├── runtime/         # Platform adapters → RuntimeAdapter trait
└── security/        # Sandboxing        → SecurityPolicy
```

## Adding Integrations

### New Provider

1. Create `src/providers/your_provider.rs`
2. Implement `Provider` trait
3. Register in `src/providers/mod.rs` factory

### New Channel

1. Create `src/channels/your_channel.rs`
2. Implement `Channel` trait
3. Register in channel factory

### New Tool

1. Create `src/tools/your_tool.rs`
2. Implement `Tool` trait (`name`, `description`, `parameters_schema`, `execute`)
3. Register in tool factory

## Pull Request Checklist

- [ ] `cargo fmt --check` and `cargo clippy` pass
- [ ] `cargo test --locked` passes
- [ ] New code has tests
- [ ] No new dependencies unless necessary
- [ ] README/docs updated if adding user-facing features
- [ ] No secrets or personal data in code/tests/commits

## Commit Convention

[Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add DashScope provider
fix: path traversal edge case
docs: update configuration guide
chore: bump tokio to 1.43
```

## Code Style

- **Minimal dependencies** — every crate adds to binary size
- **Trait-first** — define the trait, then implement
- **No unwrap in production** — use `?`, `anyhow`, or `thiserror`
- **Security by default** — sandbox, allowlist, never blocklist

## Reporting Issues

- **Bugs**: Include OS, Rust version, steps to reproduce
- **Features**: Describe use case, propose which trait to extend
- **Security**: See [SECURITY.md](SECURITY.md)

## License

By contributing, you agree your contributions will be dual-licensed under MIT and Apache-2.0.
