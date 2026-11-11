# CLAUDE.md — PRX Rust Production Code Standards

This file is loaded by Claude Code on every session. These rules are MANDATORY.

## Build & Test

Module flow: the unit of work is one module, built by one lane on one branch and delivered in one PR with the whole module done. Settle every open design value before coding (design, protocol, crypto and security modules get a card audit first); stop and ask instead of inventing one. Push only checkpoint commits while coding; open the PR (not a draft) when the module is complete.

When the module is written, run these local gates on a warm, persistent target, scoped to what changed:

```bash
source ~/.cargo/env
cargo fmt --all
cargo clippy --all-targets -- -D warnings          # add --all-features only for feature-gated changes
cargo test --lib <module::path>                    # the task's named tests + the touched module
cargo test --test architecture_boundaries          # fast boundary guard
```

Add the module's real-component tests (a real process and real wire bytes; never a shared store or synthetic bytes), and for every key fix a break-the-fix check: break the production code and show the test fails.

Do not run the full `cargo test --all-features` suite, mutation batches or stress loops locally. PR CI is the full gate.

## Seven Iron Rules (Strictly Enforced)

1. **NO panic-capable unwrapping** — `.unwrap()`, `.expect()`, and any shorthand that can panic are BANNED in production code. Use `?`, `unwrap_or`, `if let`, or explicit error returns.
2. **NO dead code** — No unused variables, parameters, or imports. Code must compile with zero warnings. `#[allow(dead_code)]` is not a fix.
3. **NO incomplete implementations** — `todo!()`, `unimplemented!()`, placeholder returns, and empty match arms are BANNED. Every code path must be fully implemented.
4. **Business logic must be verifiable** — All code must pass `cargo check`. No speculative interfaces, no pseudo-implementations, no "will fix later" stubs.
5. **Validate with `cargo check` and `cargo fix`** — Do NOT use `cargo run` or `cargo build` for validation during development. Check correctness first.
6. **Explicit API and error handling** — Validate all external inputs at boundaries. Never use panic as a substitute for error branches. Return typed errors.
7. **Minimize allocations and copies** — Follow ownership and borrowing best practices. Clone only when necessary (async move, cross-thread). Prefer `&str` over `String`, `Cow` over clone, `Arc` over deep copy.

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

// ✅ Hardcoded constants (regex etc.): keep the Result, propagate on use — no exception exists:
static RE: LazyLock<Result<Regex, regex::Error>> = LazyLock::new(|| Regex::new(r"^[a-z]+$"));
let re = RE.as_ref().map_err(|e| anyhow::anyhow!("identifier regex: {e}"))?;
```

Production code has **no** panic exception: `.unwrap()`, `.expect()`, `panic!`, `unreachable!`, panicking indexing and `#[allow(clippy::unwrap_used | expect_used | panic)]` are all banned outside `#[cfg(test)]`. `panic::catch_unwind` is not an error path either: release profiles use `panic = "abort"`, so it catches nothing there (Google Rust Style Guide: banned in first-party code except as defense in depth that nothing relies on).

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
```

Broken internal invariants also return a typed error (Google guide: codebases may route even invariant violations through `Result`; PRX does).

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
- Google guide: write `unsafe` only when the same thing cannot be done safely; wrap it in a safe abstraction isolated from business logic; add a technical justification answering "why not safe?" and, if the reason is performance, cite the benchmark; document invariants of every `unsafe fn` / `unsafe trait`

### 5. SQL Injection Prevention

```rust
// ❌ BANNED:
format!("SELECT * FROM {} WHERE id = {}", table, id)
format!("DELETE FROM {qualified_table} WHERE key = '{}'", key)

// ✅ REQUIRED:
sqlx::query("SELECT * FROM users WHERE id = $1").bind(id)
// For dynamic table/schema names: validate strictly FIRST (no panic path)
fn validate_identifier(name: &str) -> Result<&str> {
    let mut chars = name.chars();
    let valid_start = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    let valid_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid_start && valid_rest && name.len() <= 63 { Ok(name) } else { bail!("invalid identifier") }
}
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

### 10. Testing Rules

The canonical panic and test rules (Google Rust Style Guide plus PRX resource limits) live in `AGENTS.md` §3.9, which every coding lane reads. Read it before writing or reviewing tests.

## Architecture Rules

- See `AGENTS.md` for full architecture protocol
- Extend via trait implementation + factory registration
- One concern per module, one concern per PR
- Branch workflow: one module per branch and per PR. While coding, push checkpoint commits to the branch only (no PR, no CI, no review mid-way). When the whole module passes the local gates, open the PR; CI is the only full gate. After CI is green, one review by a different model (code reading plus at most two break-the-fix checks); every finding, Lows included, is fixed on the same PR, with no follow-up cards. Then rebase locally onto `origin/main`; if there is no conflict, push `main` directly and delete the branch. There is no merge queue and no post-submit stage. A red `main` becomes the next task.

## Commit Style

```
feat(scope): description
fix(scope): description
refactor(scope): description
```

All repository text (code, comments, commits, docs) is English.
