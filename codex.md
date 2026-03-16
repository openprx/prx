# codex.md — PRX Rust Production Code Standards for Codex

## Build Commands
```bash
source ~/.cargo/env
cargo check --all-features
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release --all-features
```

## MANDATORY Rules

### NO .unwrap() in Production Code
- BANNED: `.unwrap()` outside `#[cfg(test)]`
- Use instead: `?`, `.context("msg")?`, `.unwrap_or_default()`, `.unwrap_or(val)`, `if let`
- ONLY exception: `.expect("BUG: reason")` on compile-time constants (LazyLock Regex, etc.)

### Error Propagation
- Prefer `?` operator with anyhow context
- NEVER silently swallow errors — log before `.ok()` or `.unwrap_or()`
- Add `tracing::warn!()` when discarding errors intentionally

### Mutex Selection
- Sync (no await while held): `parking_lot::Mutex` — no unwrap needed
- Async (lock across await): `tokio::sync::Mutex` — .lock().await
- BANNED: `std::sync::Mutex` in production code (poisons on panic)

### SQL Safety
- ALWAYS use parameterized queries: `sqlx::query("...WHERE id = $1").bind(id)`
- Dynamic identifiers: validate with `^[a-zA-Z_][a-zA-Z0-9_]{0,62}$` before use
- NEVER use format!() to build SQL with user input

### Unsafe
- Every `unsafe` block needs `// SAFETY:` comment
- Validate inputs BEFORE unsafe block
- Minimize unsafe scope

### No Secret Logging
- NEVER log tokens, API keys, passwords, auth headers
- Use `sanitize_url()` for database URLs
- Structured tracing fields preferred

### String Efficiency
- Prefer `&str` over `String` in function params when possible
- Use `Cow<'_, str>` to avoid unnecessary cloning
- Use `Arc<str>` for shared immutable strings
- Clone only when moving into async tasks or across thread boundaries

### Async Safety
- Don't hold sync locks across .await points
- Always handle errors in spawned tasks (log at minimum)
- Use tokio::sync primitives for async contexts

### Testing
- `.unwrap()` allowed in tests but `.expect("test: reason")` preferred
- No flaky timing tests — use 2x+ expected thresholds
- Test error paths, not just happy paths
- Keep tests in `#[cfg(test)]` modules

## Architecture
- Trait-driven: extend via trait impl + factory registration
- One concern per module
- Branch workflow: never push directly to main
- English in code/commits; see AGENTS.md for full protocol
