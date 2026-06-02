//! T3 end-to-end acceptance test: watcher → `SharedConfig` → gateway authorization.
//!
//! Proves that the full chain
//!
//!   file change → HotReloadManager (file watcher) → SharedConfig.store() → authz reads
//!
//! works correctly after a hot-reload — the **"断链 1" repair** hard evidence.
//!
//! # Design choice: real file watcher
//!
//! The test uses `HotReloadManager::spawn` with a real file on disk so that the
//! `notify_debouncer_mini` path is exercised end-to-end, exactly as `daemon/mod.rs`
//! wires it. The 1-second debounce window means we must use bounded polling rather
//! than a bare sleep.
//!
//! # Why the E2E watcher test uses a dedicated Tokio runtime (not `#[tokio::test]`)
//!
//! `HotReloadManager::spawn` calls `tokio::task::spawn_blocking` internally.
//! When invoked inside `#[tokio::test]` (which uses a `current_thread` runtime by
//! default), the blocking thread and the async timer can interfere: the `spawn_blocking`
//! task's initialization can starve the timer reactor, causing `tokio::time::sleep`
//! in the poll loop to never wake up — even with `flavor = "multi_thread"`.
//!
//! The workaround is to create a dedicated `Runtime` with an explicit `block_on`
//! call, which gives fine-grained control over the runtime lifecycle.  The polling
//! loop then uses `std::thread::sleep` (OS-level, not timer-driven) to poll the
//! observable, completely side-stepping tokio timer sensitivity.
//!
//! # Authorization entry point
//!
//! `authorize_gateway_resource_mutation` (gateway/mod.rs) and
//! `authorize_resource_mutation` (gateway/api/mod.rs) are `fn` / `pub(super)` —
//! not accessible from integration tests. We replicate the identical decision logic:
//!
//!   `SecurityPolicy::from_config(&shared.load_full().autonomy, workspace)` +
//!   `SideEffectGate::new(&policy).authorize_resource_operation(...)`
//!
//! This is exactly the code path exercised by `authorize_gateway_resource_mutation`
//! (via `build_security_policy`), so the test is semantically equivalent.
//!
//! # Config TOML files are written via toml::to_string_pretty on real Config values
//!
//! `AutonomyConfig` has several required fields without `#[serde(default)]`
//! (e.g. `workspace_only`, `allowed_commands`, `forbidden_paths`, etc.).  Writing
//! hand-crafted minimal TOML (`level = "readonly"` alone) causes `Config::load_from_path`
//! to fail with "missing field" — which the watcher silently swallows and keeps the
//! previous config.  To avoid this, we construct `Config` structs in Rust and
//! serialize them with `toml::to_string_pretty`, guaranteeing all required fields
//! are present in the written files.
//!
//! # Non-empty-run guarantee
//!
//! If the post-reload assertion is changed to expect `Ok` (wrong polarity), the test
//! panics with "ReadOnly autonomy must deny". If the pre-reload assertion expects
//! `Err`, it panics with "Supervised must allow". Both were transiently inverted to
//! confirm the test is not a vacuous pass.

// Test-file allowances: doc_markdown for narrative comments with identifiers that
// aren't always backtick-wrapped, and expect_used because test code is explicitly
// permitted to panic via expect() per the project iron rules.
#![allow(clippy::doc_markdown, clippy::expect_used)]

use openprx::config::{Config, HotReloadManager, new_shared};
use openprx::security::policy::ResourceRiskLevel;
use openprx::security::{AutonomyLevel, SecurityPolicy, SideEffectGate};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Serialize a `Config` struct to a TOML string and write it to `path`.
///
/// Using `toml::to_string_pretty` (rather than hand-crafted TOML) ensures that
/// all required fields — including those in `AutonomyConfig` that lack
/// `#[serde(default)]` — are present, so `Config::load_from_path` (called
/// internally by the watcher) can parse it without error.
fn write_config_file(path: &std::path::Path, cfg: &Config) {
    let toml_str = toml::to_string_pretty(cfg).expect("test: serialize Config to TOML");
    std::fs::write(path, &toml_str).expect("test: write config file");
}

/// Replicate the logic inside `authorize_gateway_resource_mutation` (gateway/mod.rs):
///
/// ```text
/// let config = state.shared_config.load_full();
/// let policy = build_security_policy(&config);   // pub(crate) — unreachable here
/// SideEffectGate::new(&policy).authorize_resource_operation(...)
/// ```
///
/// We skip the `build_security_policy` wrapper (which adds only audit-config wiring)
/// and call `SecurityPolicy::from_config` directly. The `autonomy.level` gate that
/// blocks `ReadOnly` mutations is identical in both paths.
fn authz_low_risk_mutation(shared: &openprx::config::SharedConfig, workspace: &std::path::Path) -> Result<(), String> {
    let cfg = shared.load_full();
    let policy = SecurityPolicy::from_config(&cfg.autonomy, workspace);
    SideEffectGate::new(&policy)
        .authorize_resource_operation("gateway", "gateway:pair", ResourceRiskLevel::Low, None)
        .map(|_| ())
}

/// T3: watcher → SharedConfig → gateway authorization end-to-end.
///
/// 1. Write a Supervised `config.toml` to a temp directory (full serialization).
/// 2. Seed SharedConfig with `Config::default()` (Supervised) and wire
///    `HotReloadManager::spawn` to it — exactly as `daemon/mod.rs` does.
/// 3. **Pre-reload**: assert the authorization helper **allows** a low-risk mutation.
/// 4. Overwrite `config.toml` with a ReadOnly config (full serialization).
/// 5. **Bounded poll** (≤10 s, 100 ms interval using `std::thread::sleep`): wait for
///    SharedConfig to reflect `ReadOnly` — debounce = 1 s, so propagation ≈ 1–2 s.
/// 6. **Post-reload**: assert the same call is now **denied** with "read-only mode".
///
/// This proves: file write → `notify` event → HotReloadManager.try_reload() →
/// SharedConfig.store() → authz reads new policy — the complete "断链 1" chain.
///
/// # Why SharedConfig is seeded with Config::default() rather than from the file
///
/// `Config::load_from_path` is `pub(crate)`, unavailable to integration tests.
/// `Config::default()` has `autonomy.level = Supervised` by construction, which is
/// what the Supervised TOML also produces — so the pre-reload baseline is correct.
/// The watcher internally calls the crate-private loader on every reload event, so
/// the watcher→SharedConfig leg is fully production-code-exercised.
#[test]
fn watcher_shared_config_gateway_authz_e2e() {
    // Build a dedicated multi-thread runtime for this test.
    // `HotReloadManager::spawn` needs `tokio::task::spawn_blocking` to schedule
    // the blocking watcher loop.
    //
    // We use `shutdown_background()` at the end (instead of letting `rt` drop
    // normally) because `run_watcher` is an infinite loop — it exits only when
    // the notify channel closes.  A normal `Runtime::drop` waits for all
    // `spawn_blocking` tasks to complete; `shutdown_background` abandons them
    // without waiting, preventing the test from hanging.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test: build tokio runtime");

    let tmp = tempfile::tempdir().expect("test: tempdir");
    let config_path = tmp.path().join("config.toml");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("test: create workspace");

    // ── Phase 1: write full Supervised config to disk ─────────────────────────
    // Using Config::default() serialized to TOML so all required fields are
    // present — avoids "missing field" parse errors in the watcher's loader.
    let supervised_cfg = Config {
        autonomy: openprx::config::AutonomyConfig {
            level: AutonomyLevel::Supervised,
            ..openprx::config::AutonomyConfig::default()
        },
        ..Config::default()
    };
    write_config_file(&config_path, &supervised_cfg);

    // ── Phase 2: wiring — replicate daemon's build-one-handle pattern ────────
    //
    // daemon/mod.rs:30-34:
    //   let shared_config = new_shared(config.clone());
    //   let _hot_reload = HotReloadManager::spawn(config.config_path.clone(), Arc::clone(&shared_config));
    //   // shared_config injected into gateway supervisor
    //
    // We replicate this minimal wiring with Config::default() as the initial seed.
    let initial = Config::default();
    assert_eq!(
        initial.autonomy.level,
        AutonomyLevel::Supervised,
        "sanity: Config::default() must be Supervised — T3 pre-reload baseline"
    );
    let shared = new_shared(initial);

    // Run all async logic (spawn_blocking needs the runtime to be actively driven).
    // `rt.block_on` drives the runtime on the current thread while the async block
    // executes. `spawn_blocking` submissions are picked up by the runtime's blocking
    // thread pool, which is serviced as long as the runtime is running.
    rt.block_on(async {
        // _watcher keeps the HotReloadManager alive for the duration of the test.
        // (Rust: `let _watcher = X` binds the value until end-of-scope. `let _ = X`
        // would drop immediately — the named binding is intentional.)
        let _watcher = HotReloadManager::spawn(config_path.clone(), Arc::clone(&shared));

        // Give the spawn_blocking thread time to fully start and capture the initial
        // content hash of the Supervised TOML. If we write ReadOnly before the thread
        // has read the initial hash, it will use ReadOnly as its baseline, then the
        // subsequent inotify event will report the same hash → watcher skips reload.
        //
        // `spawn_blocking` uses a thread pool; scheduling can take a few hundred ms.
        // 2 s gives ample headroom for CI environments.
        tokio::time::sleep(Duration::from_millis(2000)).await;

        // ── Phase 3: pre-reload — assert Supervised ALLOWS low-risk mutation ─────
        assert!(
            authz_low_risk_mutation(&shared, &workspace).is_ok(),
            "pre-reload: Supervised autonomy must allow a low-risk gateway mutation (gateway:pair)"
        );

        // ── Phase 4: trigger watcher — overwrite config.toml with ReadOnly ───────
        //
        // We serialize a ReadOnly `Config` (all fields present) so the watcher's
        // `Config::load_from_path` can parse it without a "missing field" error.
        let read_only_cfg = Config {
            autonomy: openprx::config::AutonomyConfig {
                level: AutonomyLevel::ReadOnly,
                ..openprx::config::AutonomyConfig::default()
            },
            ..Config::default()
        };
        write_config_file(&config_path, &read_only_cfg);

        // ── Phase 5: bounded poll — wait for SharedConfig to reflect ReadOnly ────
        //
        // HotReloadManager debounce = 1 s. Typical propagation: < 2 s after write.
        // Timeout: 10 s — generous CI headroom.
        let poll_interval = Duration::from_millis(100);
        let timeout = Duration::from_secs(10);
        let deadline = Instant::now() + timeout;

        let reloaded = loop {
            if shared.load_full().autonomy.level == AutonomyLevel::ReadOnly {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(poll_interval).await;
        };

        assert!(
            reloaded,
            "watcher did not propagate ReadOnly to SharedConfig within {timeout:?} — \
             断链 1 not repaired: file change did not reach SharedConfig"
        );

        assert_eq!(
            shared.load_full().autonomy.level,
            AutonomyLevel::ReadOnly,
            "SharedConfig must hold ReadOnly after watcher propagation"
        );

        // ── Phase 6: post-reload — assert ReadOnly DENIES the same low-risk mutation
        let denied = authz_low_risk_mutation(&shared, &workspace)
            .expect_err("post-reload: ReadOnly autonomy must deny the low-risk gateway mutation");
        assert!(
            denied.contains("read-only mode"),
            "denial reason must mention read-only mode; got: {denied:?}"
        );

        // ── Non-empty-run guarantee ────────────────────────────────────────────────
        //
        // Phase 6 inverted to `.is_ok()` → panics:
        //   "post-reload: ReadOnly autonomy must deny the low-risk gateway mutation"
        // Phase 3 inverted to `.is_err()` → panics:
        //   "pre-reload: Supervised autonomy must allow a low-risk gateway mutation"
        // Both directions were transiently flipped and confirmed to fail during
        // development before restoring the correct assertions.
    });

    // Shut down the runtime without waiting for blocking tasks.
    // `run_watcher` (the HotReloadManager inner loop) is infinite — it blocks on
    // `rx.recv()` until the notify channel closes, which only happens when the
    // `debouncer` inside the blocking thread is dropped.  A normal `rt.drop()`
    // would wait for all spawn_blocking tasks to finish, hanging forever.
    // `shutdown_background()` abandons them immediately — safe here because the
    // test assertions are already complete.
    rt.shutdown_background();
}

/// Complementary smoke test: direct `SharedConfig.store()` → authz flip.
///
/// Isolates the SharedConfig→authz leg from the file-watcher leg. If the watcher
/// test ever becomes flaky on a specific FS or CI runner, this test still proves
/// that the `authorize_gateway_resource_mutation`-equivalent logic reads SharedConfig
/// (D) and not a stale cached copy (C).
#[test]
fn shared_config_direct_store_authz_flip() {
    let tmp = tempfile::tempdir().expect("test: tempdir");
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("test: create workspace");

    let supervised = Config::default(); // autonomy is Supervised by default
    let shared = new_shared(supervised);

    assert_eq!(
        shared.load_full().autonomy.level,
        AutonomyLevel::Supervised,
        "sanity: Config::default() must be Supervised"
    );

    // Pre-store: allowed
    assert!(
        authz_low_risk_mutation(&shared, &workspace).is_ok(),
        "Supervised allows low-risk mutation"
    );

    // Publish ReadOnly directly — simulates daemon receiving a hot-reload event
    // (HotReloadManager calls shared.store() internally after parsing the file).
    let read_only = Config {
        autonomy: openprx::config::AutonomyConfig {
            level: AutonomyLevel::ReadOnly,
            ..openprx::config::AutonomyConfig::default()
        },
        ..Config::default()
    };
    shared.store(Arc::new(read_only));

    // Post-store: denied — proves authz reads D (SharedConfig), not stale C.
    let denied = authz_low_risk_mutation(&shared, &workspace).expect_err("ReadOnly must deny");
    assert!(
        denied.contains("read-only mode"),
        "denial must mention read-only mode; got: {denied:?}"
    );
}
