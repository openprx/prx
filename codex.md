# codex.md — PRX Rust Production Code Standards for Codex

## Rust Edition: 2024

## Build Commands
```bash
source ~/.cargo/env
cargo check --all-features
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release --all-features
```

## Seven Iron Rules (Strictly Enforced)
1. NO panic-capable unwrapping — .unwrap(), .expect(), any panic shorthand BANNED in production code
2. NO dead code — zero unused variables, parameters, imports. Zero warnings.
3. NO incomplete implementations — todo!(), unimplemented!(), placeholder returns, empty match arms BANNED
4. Business logic must be verifiable — must pass cargo check, no speculative interfaces
5. Validate with cargo check and cargo fix — not cargo run/build
6. Explicit error handling — validate external inputs at boundaries, never panic instead of error
7. Minimize allocations — prefer &str over String, Cow over clone, Arc over deep copy, clone only when necessary

## MANDATORY Rules

### NO .unwrap()/.expect() in Production Code
- BANNED: `.unwrap()` and `.expect()` outside `#[cfg(test)]`
- Use instead: `?`, `.context("msg")?`, `.unwrap_or_default()`, `.unwrap_or(val)`, `if let`
- For compile-time constants in LazyLock: use a safe init function that returns Result, not .expect()

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
