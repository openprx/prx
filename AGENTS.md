# Repository Guidelines

## Project Structure & Module Organization
- Core Rust code lives in `src/`, organized by subsystem: `agent/`, `channels/`, `providers/`, `tools/`, `memory/`, `security/`, `gateway/`, and `config/`.
- Integration tests are in `tests/`; focused helpers and scripts are in `test_helpers/` and `scripts/`.
- Examples for extension points are in `examples/` (custom provider/tool/memory/channel).
- Operational and design docs are in `docs/`; CI and templates are in `.github/`.
- Firmware-related assets are in `firmware/` and are separate from the core runtime path.

## Build, Test, and Development Commands
- `cargo build` — development build.
- `cargo build --release` — optimized release build.
- `cargo run -- onboard` — initialize local config/workspace.
- `cargo run -- agent` — interactive agent mode.
- `cargo run -- gateway` — start webhook gateway.
- `cargo test` — run unit + integration tests.
- `cargo clippy --all-targets -- -D warnings` — strict lint gate.
- `cargo fmt --all -- --check` — formatting check (use `cargo fmt` to fix).

## Coding Style & Naming Conventions
- Use Rust 2021 idioms and keep code `rustfmt`-clean.
- Naming: modules/files `snake_case`, types/traits `PascalCase`, functions/variables `snake_case`, constants `UPPER_SNAKE_CASE`.
- Prefer small, composable modules; keep cross-module interfaces explicit in `mod.rs` and config schema.
- Avoid introducing new warnings; treat clippy and formatting as required CI quality gates.

## Testing Guidelines
- Place unit tests near implementation (`#[cfg(test)]`), and cross-module behavior tests in `tests/`.
- Name tests by behavior, e.g. `rejects_empty_message`, `config_toml_roundtrip`.
- Run targeted tests while iterating: `cargo test tools::mcp:: -- --nocapture`.
- Ensure changed paths have direct coverage plus one regression/edge-case test when practical.

## Commit & Pull Request Guidelines
- Follow Conventional Commit style seen in history: `fix(scope): ...`, `docs(scope): ...`, `ci: ...`.
- Keep commits focused and atomic; include scope when meaningful (e.g. `fix(channel): ...`).
- PRs should include: problem, rationale, scope boundary, linked issues, validation commands/results, and risk notes.
- Use `.github/pull_request_template.md`; explicitly document skipped checks and rollback plan.

## Security & Configuration Tips
- Never commit secrets; use `.env.example` patterns and local config under `~/.zeroclaw/`.
- For new integrations/tools, document permission/network impact and default to least privilege.
- Config changes must be reflected in `src/config/schema.rs` defaults and serialization roundtrip behavior.
