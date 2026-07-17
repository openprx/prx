# Contributing to OpenPRX

Thanks for your interest in contributing!

## Development Setup

```bash
git clone https://github.com/openprx/prx.git
cd prx

# Normal local development loop
cargo fmt --all
cargo fmt --all -- --check
cargo check -p openprx --all-features
cargo test -p openprx <affected-test-filter>
```

The normal local loop must run focused tests that execute at least one test.
The full engineering gate is reserved for a GitHub delivery, an explicitly
authorized release/deploy, or a comprehensive audit:

```bash
cargo test --locked
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --test architecture_boundaries
cargo deny check
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

- [ ] `cargo fmt --all -- --check` and `cargo check -p openprx --all-features` pass
- [ ] Focused affected tests ran and executed at least one test
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes
- [ ] `cargo test --locked` passes
- [ ] Architecture boundaries and `cargo deny check` pass
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
