# Contributing to OpenPRX

Thanks for your interest in contributing!

## Development Setup

```bash
git clone https://github.com/openprx/prx.git
cd prx

# Local checks before opening a pull request, scoped to what you changed
cargo fmt --all
cargo clippy --all-targets -- -D warnings          # add --all-features only for feature-gated changes
cargo test --lib <module::path>                    # the tests you added or changed, plus the touched module
cargo test --test architecture_boundaries          # fast architecture boundary guard
```

Keep the local loop scoped: the focused tests must execute at least one test.
Do not run the full test suite, an all-features workspace clippy or
`cargo deny` locally as a precondition for a pull request. Pull request CI is
the only full gate; it runs the complete test suite and the all-features lint.

## Source file size

Keep each first-party source file at or below 5,000 physical lines, including
comments and tests. Split code by responsibility rather than moving unrelated
logic merely to satisfy the limit. Run `python3 scripts/ci/source_size_gate.py`
after adding files to Git; the Contracts job checks all tracked source files,
including build definitions and executable scripts. The vendored Matrix SDK is
separately identified by its upstream provenance. Untracked files are not part
of the gate until added to Git.

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

- [ ] `cargo fmt --all` leaves no diff
- [ ] `cargo clippy --all-targets -- -D warnings` passes (with `--all-features` only for feature-gated changes)
- [ ] The tests you added or changed and the touched module's tests ran and executed at least one test
- [ ] `cargo test --test architecture_boundaries` passes
- [ ] Pull request CI (the full gate) is green
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
