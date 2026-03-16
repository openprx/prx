# CLAUDE.md — PRX Rust Production Code Standards

This file is loaded by Claude Code on every session. These rules are MANDATORY.

## Build & Test

```bash
source ~/.cargo/env
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --all-features  # QA/production build
```

## Rust Safety Rules (Production-Grade, Non-Negotiable)

### 1. ZERO .unwrap() in Non-Test Code

**BANNED:** `.unwrap()` anywhere outside `#[cfg(test)]` modules.

```rust
// ❌ BANNED — panics in production
let val = some_option.unwrap();
let parsed: i32 = text.parse().unwrap();
let guard = mutex.lock().unwrap();
let json = serde_json::to_string(&obj).unwrap();

// ✅ REQUIRED alternatives:
let val = some_option.ok_or_else(|| anyhow::anyhow!("missing value for X"))?;
let val = some_option.unwrap_or_default();
let val = some_option.unwrap_or_else(|| fallback);
let parsed: i32 = text.parse().unwrap_or(0);
let guard = mutex.lock();  // parking_lot — no poison, no unwrap needed
let json = serde_json::to_string(&obj)?;

// ✅ ONLY exception — compile-time constants in LazyLock/OnceLock:
static RE: LazyLock<Regex> = LazyLock::new(||
    Regex::new(r"^[a-z]+$").expect("BUG: invalid hardcoded regex for identifier pattern")
);
// .expect() with descriptive message is allowed ONLY for compile-time-constant values
```

### 2. Error Handling Hierarchy

```
?                          → preferred (propagate with context)
.context("msg")?           → add context to anyhow errors
.map_err(|e| ...)?         → transform error types
.unwrap_or_default()       → safe fallback for non-critical values
.unwrap_or(fallback)       → explicit fallback
.unwrap_or_else(|| ...)    → computed fallback
if let Some(v) = opt { }   → pattern match
.ok()                      → discard error when truly ignorable (log first!)
.expect("BUG: ...")        → ONLY for invariants that are programming errors
```

**NEVER** silently swallow errors. If using `.ok()` or `.unwrap_or()`, add a `tracing::warn!()` or comment explaining why.

### 3. Mutex Rules

```rust
// ✅ Sync (no .await while holding):
use parking_lot::Mutex;        // No poison, no unwrap needed
use parking_lot::RwLock;       // Reader-writer variant

// ✅ Async (lock held across .await):
use tokio::sync::Mutex;        // .lock().await — no unwrap needed
use tokio::sync::RwLock;

// ❌ BANNED in production code:
use std::sync::Mutex;          // Poisons on panic, requires .unwrap()
use std::sync::RwLock;         // Same issue
```

`std::sync::Mutex` is ONLY allowed in `#[cfg(test)]` modules.

### 4. Unsafe Rules

- `unsafe` blocks require `// SAFETY:` comment explaining why it's sound
- Minimize unsafe scope to the smallest possible block
- Validate all inputs BEFORE the unsafe block
- Prefer safe abstractions (`nix` crate over raw `libc`, etc.)

### 5. SQL Injection Prevention

```rust
// ❌ BANNED:
format!("SELECT * FROM {} WHERE id = {}", table, id)
format!("DELETE FROM {qualified_table} WHERE key = '{}'", key)

// ✅ REQUIRED:
sqlx::query("SELECT * FROM users WHERE id = $1").bind(id)
// For dynamic table/schema names: validate with strict regex FIRST
fn validate_identifier(name: &str) -> Result<&str> {
    let re = Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_]{0,62}$").unwrap();
    if re.is_match(name) { Ok(name) } else { bail!("invalid identifier") }
}
```

### 6. String Handling

```rust
// ❌ Wasteful:
fn process(data: String) { ... }        // forces caller to clone
format!("{}", simple_string)             // use .to_string() or clone

// ✅ Efficient:
fn process(data: &str) { ... }          // borrow when possible
fn process(data: impl AsRef<str>) { }   // flexible
fn process(data: Cow<'_, str>) { }      // avoid clone when not needed
Arc<str>                                 // shared immutable strings
```

### 7. Async Best Practices

```rust
// ❌ BAD — blocks async runtime:
let guard = parking_lot_mutex.lock();
expensive_sync_operation();
drop(guard);
// ... .await point after

// ✅ GOOD — use tokio::sync for async contexts:
let guard = tokio_mutex.lock().await;

// ❌ BAD — spawning without error handling:
tokio::spawn(async { risky_operation().await });

// ✅ GOOD:
tokio::spawn(async {
    if let Err(e) = risky_operation().await {
        tracing::error!("background task failed: {e}");
    }
});
```

### 8. Logging & Observability

```rust
// ❌ NEVER log secrets:
tracing::info!("token: {}", api_key);
tracing::debug!("auth header: {}", auth);

// ✅ Sanitize:
tracing::info!("connecting to {}", sanitize_url(&database_url));
tracing::debug!("auth: [REDACTED]");

// Use structured fields:
tracing::info!(host = %hostname, port = port, "server started");
```

### 9. Clippy Lints (Enforce in CI)

Add to `Cargo.toml` or `clippy.toml`:
```toml
# In lib.rs or main.rs:
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]  # or warn if using expect for constants
#![deny(clippy::panic)]
#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]
```

### 10. Testing Rules

- Tests MAY use `.unwrap()` — but prefer `.expect("test: description")` for better failure messages
- Tests use `#[cfg(test)]` module isolation
- No flaky timing tests — use reasonable thresholds (2x expected) or mock time
- Test error paths, not just happy paths

## Architecture Rules

- See `AGENTS.md` for full architecture protocol
- Extend via trait implementation + factory registration
- One concern per module, one concern per PR
- Branch workflow: never push directly to `main`

## Commit Style

```
feat(scope): description
fix(scope): description
refactor(scope): description
```

English only in commits and code comments. Chinese allowed in user-facing docs.
