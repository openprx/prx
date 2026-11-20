# CLAUDE.md — PRX Rust Production Code Standards

This file is loaded by Claude Code on every session. These rules are MANDATORY.

## Build & Test

Module flow: the unit of work is one module, built by one lane on one branch and landed whole. The controller settles every open design value in the design document before coding; a lane stops and asks instead of inventing one. Commit checkpoints on the branch while coding; the lane does not push, open a PR or land.

When the module is written, run these local gates on a warm, persistent target, scoped to what changed:

```bash
source ~/.cargo/env
cargo fmt --all
cargo clippy --lib --bins -- -D warnings           # inner loop: production code only
cargo clippy --all-targets --all-features -- -D warnings   # once, before reporting: also type-checks the existing tests
scripts/ci/build_architecture_boundaries.sh /tmp/arch && /tmp/arch   # boundary scanners, built standalone (seconds, not a crate build)
```

A lane runs no `cargo test`: building the test harness of this crate takes about 20 minutes and 20 GB. The existing suite runs once, in the controller's local full gate.

Do not write test code (owner 2026-11-20): a lane writes business logic only. It keeps the existing tests compiling and passing with the smallest mechanical edit, deletes a test that asserts behaviour the change intentionally replaces, and lists every such edit in its report. A change is verified by a real test: a debug `prx` built from the branch, run against real providers on an isolated state directory. After the module lands, it gets a real test again on a binary built from main; a failure is the next task.

A lane does not run the whole-workspace suite, mutation batches or stress loops. The full gate is the controller's local full gate (the ci-run.yml gate set plus supply-chain checks, run on the build hosts); it runs once on the finished, rebased branch.

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
- Branch workflow: one module per branch. The lane writes no test code; it runs the scoped gates above (no `cargo test`), then reports, and the branch gets its real test. The controller rebases the branch onto `origin/main` and runs the local full gate; when it is green, `main` is pushed directly (fast-forward) and the branch is deleted. There is no PR, no code review after coding, no merge queue and no post-submit stage; GitHub CI runs on `main` after the push and covers what the local gate cannot (Windows). A change to Windows-only code, installers or `.github/workflows` goes through a PR and GitHub CI first. Right after landing, run the module's real test against a binary built from `main`. A red `main` or a failed real test becomes the next task. Three strikes: the same failure signature a third time stops all retries and guess-fixes; write a root-cause note naming the violated invariant, fix against it, and turn the lesson into a flow rule the same day. Compare an infra-looking CI failure (exit 143, OOM, timeout) with main's config and a passing PR's command before calling it a flake.

## Commit Style

```
feat(scope): description
fix(scope): description
refactor(scope): description
```

All repository text (code, comments, commits, docs) is English.
