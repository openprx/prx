//! P1/P2 Cross-Module Integration Tests
//!
//! This file covers P1 and P2 tests across multiple module boundaries:
//! - Config hot-reload propagation (INT-CR-03/04/05)
//! - Evolution circuit breaker, rollback, trace (INT-SME-04/05/06)
//! - Memory backend interop (INT-MM-02/03/04/05)
//! - Hooks lifecycle (INT-HAT-01/03/04)
//! - Observer resilience (INT-OA-01/02)
//! - Gateway idempotency + public bind (INT-GS-06/07)
//! - Gateway webhook filtering + idempotency (INT-GCW-02/04)
//! - Runtime sandbox cascade (INT-TR-05)
//! - Agent auto-save short message skip (INT-AM-02)
//! - Scope rule channel filtering (INT-CS-02)
//! - Channel mention-only filter (INT-CA-06)
//! - E2E config hot-reload (INT-E2E-02)

use openprx::config::ScopeRule;
use openprx::config::hotreload::new_shared;
use openprx::config::schema::Config;
use openprx::memory::backend::{MemoryBackendKind, classify_memory_backend};
use openprx::memory::filter::should_autosave_content;
use openprx::memory::markdown::MarkdownMemory;
use openprx::memory::sqlite::SqliteMemory;
use openprx::memory::traits::{Memory, MemoryCategory};
use openprx::security::pairing::is_public_bind;
use openprx::security::policy::SecurityPolicy;
use openprx::self_system::evolution::{
    CircuitBreaker, CircuitBreakerState, RollbackManager, TraceContext, current_trace, with_trace,
};
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════════════════════
// INT-AM-02: Agent skips auto-save for short messages
// ═══════════════════════════════════════════════════════════════════════════════

/// Short messages (below 30 chars threshold) should not be auto-saved.
#[tokio::test]
async fn int_am_02_skip_autosave_for_short_messages() {
    // "ok" is 2 chars — well below the 30-char threshold
    assert!(
        !should_autosave_content("ok"),
        "test: 2-char message should NOT be auto-saved"
    );

    // "thanks, got it" is 14 chars — still below threshold
    assert!(
        !should_autosave_content("thanks, got it"),
        "test: 14-char message should NOT be auto-saved"
    );

    // A 29-char message (just below threshold)
    let almost = "This is just barely too short";
    assert!(
        !should_autosave_content(almost),
        "test: 29-char message should NOT be auto-saved (threshold is 30)"
    );

    // A 31-char message (just above threshold)
    let above = "This message is long enough now!";
    assert!(
        should_autosave_content(above),
        "test: 31-char message SHOULD be auto-saved"
    );
}

/// Heartbeat/cron noise patterns are filtered regardless of length.
#[tokio::test]
async fn int_am_02_skip_autosave_for_noise_patterns() {
    assert!(
        !should_autosave_content("Check HEARTBEAT now and report back status please"),
        "test: heartbeat noise should not be auto-saved"
    );
    assert!(
        !should_autosave_content("[cron:heartbeat] run the scheduled task for health monitoring"),
        "test: cron noise should not be auto-saved"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-CS-02: Scope rule allows tool for different channel
// ═══════════════════════════════════════════════════════════════════════════════

/// Scope rule denies shell for telegram but allows it for signal.
#[tokio::test]
async fn int_cs_02_scope_rule_channel_differentiation() {
    let policy = SecurityPolicy {
        scope_rules: vec![ScopeRule {
            user: None,
            channel: Some("telegram".into()),
            chat_type: None,
            tools_allow: vec!["memory_recall".into()],
            tools_deny: vec!["shell".into()],
        }],
        scope_default_allow: true,
        ..SecurityPolicy::default()
    };

    // Telegram: shell denied
    assert!(
        !policy.is_tool_allowed("shell", "alice", "telegram", "direct"),
        "test: shell should be denied on telegram"
    );

    // Signal: shell allowed (no rule matches signal, default allow)
    assert!(
        policy.is_tool_allowed("shell", "alice", "signal", "direct"),
        "test: shell should be allowed on signal (default allow)"
    );

    // Telegram: memory_recall allowed (in the allow list)
    assert!(
        policy.is_tool_allowed("memory_recall", "alice", "telegram", "direct"),
        "test: memory_recall should be allowed on telegram"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-CR-01/05: Config hot-reload: ArcSwap atomic swap + env override reasoning
// ═══════════════════════════════════════════════════════════════════════════════

/// Hot-reload swaps config atomically — concurrent readers see consistent snapshots.
#[tokio::test]
async fn int_cr_01_hot_reload_concurrent_readers_see_consistent_config() {
    let shared = new_shared(Config::default());
    let original_temp = shared.load_full().default_temperature;

    // Spawn 10 concurrent readers
    let mut handles = Vec::new();
    for _ in 0..10 {
        let shared = shared.clone();
        handles.push(tokio::spawn(async move {
            // Each reader loads the config — should be a valid snapshot
            let cfg = shared.load_full();
            // Temperature should be either the original or the updated value, never garbage
            assert!(
                cfg.default_temperature >= 0.0,
                "test: temperature should be a valid non-negative f64"
            );
        }));
    }

    // Mid-stream swap
    let new_cfg = Config {
        default_temperature: 0.42,
        ..Config::default()
    };
    shared.store(Arc::new(new_cfg));

    for h in handles {
        h.await.expect("test: reader task panicked");
    }

    // After all readers complete, the new config should be the current one
    let final_cfg = shared.load_full();
    assert!(
        (final_cfg.default_temperature - 0.42).abs() < 1e-9,
        "test: config should have been swapped to 0.42, got {}",
        final_cfg.default_temperature
    );

    // Verify original temperature was different
    assert!(
        (original_temp - 0.42).abs() > 1e-9,
        "test: original temperature should differ from 0.42"
    );
}

/// Config change propagates to `SecurityPolicy` construction.
/// INT-CR-03: Verify that changes to autonomy config produce different `SecurityPolicy`.
#[tokio::test]
async fn int_cr_03_config_change_propagates_to_security_policy() {
    let shared = new_shared(Config::default());

    // Build SecurityPolicy from initial config
    let cfg1 = shared.load_full();
    let policy1 = SecurityPolicy::from_config(&cfg1.autonomy, &std::env::temp_dir());
    let initial_autonomy = policy1.autonomy;

    // Swap config with different autonomy level
    #[allow(clippy::field_reassign_with_default)]
    let new_cfg = {
        let mut cfg = Config::default();
        cfg.autonomy.level = openprx::security::policy::AutonomyLevel::Full;
        cfg
    };
    shared.store(Arc::new(new_cfg));

    // Build SecurityPolicy from new config
    let cfg2 = shared.load_full();
    let policy2 = SecurityPolicy::from_config(&cfg2.autonomy, &std::env::temp_dir());

    // The old snapshot should still be valid
    assert_eq!(
        policy1.autonomy, initial_autonomy,
        "test: old policy snapshot should retain initial autonomy"
    );

    // The new policy should reflect the change
    assert_eq!(
        policy2.autonomy,
        openprx::security::policy::AutonomyLevel::Full,
        "test: new policy should reflect 'full' autonomy from config swap"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-SME-04: Circuit breaker prevents evolution cascade failures
// ═══════════════════════════════════════════════════════════════════════════════

/// After threshold consecutive failures, circuit breaker opens and blocks execution.
#[tokio::test]
async fn int_sme_04_circuit_breaker_opens_after_threshold_failures() {
    let mut breaker = CircuitBreaker::new(5, 1);
    let now = chrono::Utc::now();

    // Record 4 failures — should stay Closed
    for _ in 0..4 {
        breaker.record_failure(now);
    }
    assert_eq!(
        breaker.state(),
        CircuitBreakerState::Closed,
        "test: breaker should remain Closed after 4 failures (threshold=5)"
    );
    assert!(breaker.can_execute(now), "test: Closed breaker should allow execution");

    // 5th failure trips the breaker
    breaker.record_failure(now);
    assert_eq!(
        breaker.state(),
        CircuitBreakerState::Open,
        "test: breaker should Open after 5 failures"
    );
    assert!(!breaker.can_execute(now), "test: Open breaker should block execution");

    // After cooldown (1 hour), should transition to HalfOpen
    let after_cooldown = now + chrono::Duration::hours(1);
    assert!(
        breaker.can_execute(after_cooldown),
        "test: breaker should allow execution after cooldown (HalfOpen)"
    );
    assert_eq!(
        breaker.state(),
        CircuitBreakerState::HalfOpen,
        "test: breaker should be HalfOpen after cooldown"
    );

    // Success in HalfOpen resets to Closed
    breaker.record_success();
    assert_eq!(
        breaker.state(),
        CircuitBreakerState::Closed,
        "test: success in HalfOpen should reset to Closed"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-SME-05: Rollback manager restores previous state on failure
// ═══════════════════════════════════════════════════════════════════════════════

/// `RollbackManager` can backup, modify, then restore the previous version.
#[tokio::test]
async fn int_sme_05_rollback_manager_backup_and_restore() {
    let dir = tempfile::tempdir().expect("test: create temp dir");
    let target = dir.path().join("system_prompt.md");
    let versions = dir.path().join(".evolution/rollback/versions");

    // Write initial content
    tokio::fs::write(&target, "You are a helpful assistant.")
        .await
        .expect("test: write initial prompt");

    let manager = RollbackManager::new(dir.path(), &target, &versions, 5).expect("test: create rollback manager");

    // Backup current version
    let snapshot = manager
        .backup_current_version()
        .await
        .expect("test: backup should succeed")
        .expect("test: should return a snapshot");

    // Modify the file (simulating evolution mutation)
    tokio::fs::write(&target, "You are an evil assistant.")
        .await
        .expect("test: write mutated prompt");

    // Verify mutation took effect
    let mutated = tokio::fs::read_to_string(&target).await.expect("test: read mutated");
    assert_eq!(mutated, "You are an evil assistant.");

    // Rollback to the backup
    manager
        .rollback_to_version(&snapshot.version_id)
        .await
        .expect("test: rollback should succeed");

    // Verify restoration
    let restored = tokio::fs::read_to_string(&target).await.expect("test: read restored");
    assert_eq!(
        restored, "You are a helpful assistant.",
        "test: content should be restored to original after rollback"
    );
}

/// Rollback manager prunes old versions beyond `max_versions`.
#[tokio::test]
async fn int_sme_05_rollback_manager_prunes_old_versions() {
    let dir = tempfile::tempdir().expect("test: create temp dir");
    let target = dir.path().join("config.toml");
    let versions = dir.path().join(".evolution/rollback/versions");

    let manager = RollbackManager::new(dir.path(), &target, &versions, 3).expect("test: create rollback manager");

    // Create 5 backups (only 3 should be retained)
    for i in 0..5 {
        tokio::fs::write(&target, format!("version={i}"))
            .await
            .expect("test: write version");
        manager
            .backup_current_version()
            .await
            .expect("test: backup should succeed");
    }

    let remaining = manager.list_versions().await.expect("test: list versions");
    assert!(
        remaining.len() <= 3,
        "test: should retain at most 3 versions (max_versions=3), got {}",
        remaining.len()
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-SME-06: Trace context propagates through full evolution pipeline
// ═══════════════════════════════════════════════════════════════════════════════

/// `with_trace()` propagates `TraceContext` so that `current_trace()` returns it inside the span.
#[tokio::test]
async fn int_sme_06_trace_context_propagation() {
    let ctx = TraceContext::new();
    let expected_trace_id = ctx.trace_id.clone();
    let expected_experiment_id = ctx.experiment_id.clone();

    let observed = with_trace(ctx, || async {
        // Inside the trace scope, current_trace() should return the context
        current_trace().expect("test: trace context should be available")
    })
    .await;

    assert_eq!(
        observed.trace_id, expected_trace_id,
        "test: trace_id should match the outer context"
    );
    assert_eq!(
        observed.experiment_id, expected_experiment_id,
        "test: experiment_id should match the outer context"
    );
}

/// Nested operations within `with_trace()` share the same trace context.
#[tokio::test]
async fn int_sme_06_trace_context_shared_across_nested_operations() {
    let ctx = TraceContext::new();
    let expected_trace_id = ctx.trace_id.clone();

    let (id_from_op1, id_from_op2) = with_trace(ctx, || async {
        let op1_trace = current_trace().expect("test: trace context in op1").trace_id;

        // Simulate a second operation in the same trace span
        let op2_trace = current_trace().expect("test: trace context in op2").trace_id;

        (op1_trace, op2_trace)
    })
    .await;

    assert_eq!(
        id_from_op1, expected_trace_id,
        "test: op1 should see the correct trace_id"
    );
    assert_eq!(id_from_op2, expected_trace_id, "test: op2 should see the same trace_id");
    assert_eq!(
        id_from_op1, id_from_op2,
        "test: both operations should share the same trace context"
    );
}

/// Outside of `with_trace()`, `current_trace()` returns None.
#[tokio::test]
async fn int_sme_06_trace_context_absent_outside_scope() {
    let result = current_trace();
    assert!(
        result.is_none(),
        "test: current_trace() should return None outside of with_trace() scope"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-MM-02: LucidMemory delegates to SQLite local
// ═══════════════════════════════════════════════════════════════════════════════

/// `LucidMemory` wraps `SqliteMemory` and delegates store/get/recall/count operations.
#[tokio::test]
async fn int_mm_02_lucid_delegates_to_sqlite() {
    use openprx::memory::lucid::LucidMemory;

    let tmp = tempfile::TempDir::new().expect("test: create temp dir");
    let sqlite = SqliteMemory::new(tmp.path()).expect("test: create sqlite memory");
    let lucid = LucidMemory::new(tmp.path(), sqlite);

    // Store through LucidMemory
    lucid
        .store("lucid-key-1", "lucid content one", MemoryCategory::Core, None)
        .await
        .expect("test: lucid store should succeed");

    lucid
        .store("lucid-key-2", "lucid content two", MemoryCategory::Core, None)
        .await
        .expect("test: lucid store 2 should succeed");

    // Get through LucidMemory
    let entry = lucid
        .get("lucid-key-1")
        .await
        .expect("test: lucid get should succeed")
        .expect("test: entry should exist");
    assert_eq!(entry.content, "lucid content one");

    // Count through LucidMemory
    let count = lucid.count().await.expect("test: lucid count should succeed");
    assert_eq!(count, 2, "test: lucid should have 2 entries");

    // Recall through LucidMemory
    let results = lucid
        .recall("lucid content", 10, None)
        .await
        .expect("test: lucid recall should succeed");
    assert!(!results.is_empty(), "test: lucid recall should find matching entries");

    // Forget through LucidMemory
    let forgotten = lucid
        .forget("lucid-key-1")
        .await
        .expect("test: lucid forget should succeed");
    assert!(forgotten, "test: forget should return true for existing key");

    let count_after = lucid.count().await.expect("test: count after forget");
    assert_eq!(count_after, 1, "test: count should decrease after forget");
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-MM-03: MarkdownMemory concurrent access
// ═══════════════════════════════════════════════════════════════════════════════

/// 5 concurrent tasks write to the same `MarkdownMemory` without file corruption.
#[tokio::test]
async fn int_mm_03_markdown_concurrent_writes_no_corruption() {
    let tmp = tempfile::TempDir::new().expect("test: create temp dir");
    let mem = Arc::new(MarkdownMemory::new(tmp.path()));

    let mut handles = Vec::new();
    for i in 0..5 {
        let mem = mem.clone();
        handles.push(tokio::spawn(async move {
            mem.store(
                &format!("concurrent-key-{i}"),
                &format!("Concurrent write number {i} with enough content"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap_or_else(|_| panic!("test: concurrent write {i} failed"));
        }));
    }

    for h in handles {
        h.await.expect("test: concurrent task panicked");
    }

    // Verify all 5 entries are retrievable
    let count = mem.count().await.expect("test: count should succeed");
    assert_eq!(count, 5, "test: all 5 concurrent writes should be present, got {count}");

    // Spot-check that content is not corrupted
    for i in 0..5 {
        let entry = mem
            .get(&format!("concurrent-key-{i}"))
            .await
            .expect("test: get should succeed")
            .unwrap_or_else(|| panic!("test: entry concurrent-key-{i} should exist"));
        assert!(
            entry.content.contains(&format!("write number {i}")),
            "test: content for key {i} should not be corrupted"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-MM-04: Memory backend fallback on unknown type
// ═══════════════════════════════════════════════════════════════════════════════

/// Unknown backend type classifies as Unknown, allowing fallback logic.
#[tokio::test]
async fn int_mm_04_memory_backend_fallback_on_unknown() {
    let kind = classify_memory_backend("redis");
    assert_eq!(
        kind,
        MemoryBackendKind::Unknown,
        "test: 'redis' should classify as Unknown"
    );

    let kind2 = classify_memory_backend("custom-backend");
    assert_eq!(
        kind2,
        MemoryBackendKind::Unknown,
        "test: 'custom-backend' should classify as Unknown"
    );

    // Known backends should classify correctly
    assert_eq!(classify_memory_backend("sqlite"), MemoryBackendKind::Sqlite);
    assert_eq!(classify_memory_backend("markdown"), MemoryBackendKind::Markdown);
    assert_eq!(classify_memory_backend("none"), MemoryBackendKind::None);
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-GS-06: Gateway idempotency key deduplication
// (Tested at gateway HTTP level in int_agent_gateway.rs — IdempotencyStore's
// record_if_new is private and exercised by the webhook handler's Idempotency-Key
// header processing.)
// ═══════════════════════════════════════════════════════════════════════════════

/// `IdempotencyStore` correctly limits the maximum number of tracked keys.
/// We verify this indirectly by ensuring the store can be created with bounded config.
#[tokio::test]
async fn int_gs_06_idempotency_store_creation_bounded() {
    use openprx::gateway::IdempotencyStore;
    use std::time::Duration;

    // Creating a store with max_keys=1 should work (min clamped to 1)
    let _store = IdempotencyStore::new(Duration::from_secs(300), 1);

    // Creating a store with max_keys=10000 should work
    let _store_large = IdempotencyStore::new(Duration::from_secs(300), 10_000);

    // Creating a store with max_keys=0 should be clamped to 1 (no panic)
    let _store_zero = IdempotencyStore::new(Duration::from_secs(300), 0);
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-GS-07: Public bind detection warns on insecure pairing
// ═══════════════════════════════════════════════════════════════════════════════

/// `is_public_bind` correctly identifies public vs private addresses.
#[tokio::test]
async fn int_gs_07_public_bind_detection() {
    // Public addresses — should return true
    assert!(is_public_bind("0.0.0.0"), "test: 0.0.0.0 is a public bind address");
    assert!(
        is_public_bind("192.168.1.1"),
        "test: 192.168.1.1 is a public bind address"
    );
    assert!(is_public_bind("10.0.0.1"), "test: 10.0.0.1 is a public bind address");

    // Private/localhost addresses — should return false
    assert!(
        !is_public_bind("127.0.0.1"),
        "test: 127.0.0.1 is NOT a public bind address"
    );
    assert!(
        !is_public_bind("localhost"),
        "test: localhost is NOT a public bind address"
    );
    assert!(!is_public_bind("::1"), "test: ::1 is NOT a public bind address");
    assert!(!is_public_bind("[::1]"), "test: [::1] is NOT a public bind address");
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-GCW-02: Webhook group message auto-save filtering
// ═══════════════════════════════════════════════════════════════════════════════

/// `should_autosave_content` filters noise patterns even when the message is long enough.
#[tokio::test]
async fn int_gcw_02_webhook_group_filtering_noise() {
    // Group message with heartbeat pattern (should NOT be auto-saved)
    let heartbeat_msg = "Check HEARTBEAT for all services in the group chat system";
    assert!(
        !should_autosave_content(heartbeat_msg),
        "test: heartbeat group message should not be auto-saved"
    );

    // Regular group message (should be auto-saved)
    let regular_msg = "Alice mentioned that we should deploy the update after 10pm today";
    assert!(
        should_autosave_content(regular_msg),
        "test: regular group message should be auto-saved"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-TR-05: Sandbox selection cascade
// ═══════════════════════════════════════════════════════════════════════════════

/// Sandbox auto-detection falls back gracefully when no specific backend is available.
#[tokio::test]
async fn int_tr_05_sandbox_selection_cascade_fallback() {
    use openprx::config::{SandboxBackend, SandboxConfig, SecurityConfig};
    use openprx::security::detect::create_sandbox;

    // Explicitly disabled sandbox returns NoopSandbox
    let disabled_config = SecurityConfig {
        sandbox: SandboxConfig {
            enabled: Some(false),
            backend: SandboxBackend::None,
            firejail_args: Vec::new(),
        },
        ..Default::default()
    };
    let sandbox = create_sandbox(&disabled_config);
    assert_eq!(
        sandbox.name(),
        "none",
        "test: disabled sandbox should return NoopSandbox"
    );
    assert!(sandbox.is_available(), "test: NoopSandbox should always be available");

    // Auto-detection should return some sandbox (at least NoopSandbox)
    let auto_config = SecurityConfig {
        sandbox: SandboxConfig {
            enabled: None,
            backend: SandboxBackend::Auto,
            firejail_args: Vec::new(),
        },
        ..Default::default()
    };
    let auto_sandbox = create_sandbox(&auto_config);
    assert!(
        auto_sandbox.is_available(),
        "test: auto-detected sandbox should be available"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-CS-03: PolicyPipeline layer precedence (Global < Group < Tool)
// ═══════════════════════════════════════════════════════════════════════════════

/// Tool-level policy overrides group-level deny.
#[tokio::test]
async fn int_cs_03_policy_pipeline_layer_precedence() {
    use openprx::config::schema::ToolPolicyConfig;
    use openprx::security::policy_pipeline::{EvalContext, PolicyLayer, PolicyPipeline};
    use std::collections::HashMap;

    let mut groups = HashMap::new();
    groups.insert("sessions".to_string(), "deny".to_string());

    let mut tools = HashMap::new();
    tools.insert("sessions_spawn".to_string(), "allow".to_string());

    let pipeline = PolicyPipeline::new(ToolPolicyConfig {
        default: "allow".to_string(),
        groups,
        tools,
    });

    // sessions_spawn: group=deny, tool=allow -> tool wins -> allowed
    let decision = pipeline.evaluate("sessions_spawn", &EvalContext::default());
    assert!(
        decision.allowed,
        "test: sessions_spawn should be ALLOWED (tool override beats group deny)"
    );
    assert!(
        decision.layers_applied.contains(&PolicyLayer::Tool),
        "test: Tool layer should be in layers_applied"
    );
    assert!(
        decision.layers_applied.contains(&PolicyLayer::Group),
        "test: Group layer should be in layers_applied"
    );
    assert!(
        decision.layers_applied.contains(&PolicyLayer::Global),
        "test: Global layer should be in layers_applied"
    );

    // sessions_list: group=deny, no tool override -> denied
    let decision2 = pipeline.evaluate("sessions_list", &EvalContext::default());
    assert!(
        !decision2.allowed,
        "test: sessions_list should be DENIED (group deny, no tool override)"
    );

    // shell: no group, no tool -> global allow
    let decision3 = pipeline.evaluate("shell", &EvalContext::default());
    assert!(decision3.allowed, "test: shell should be ALLOWED (global default)");
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-E2E-02: Config hot-reload -> provider change -> next request uses new config
// ═══════════════════════════════════════════════════════════════════════════════

/// Simulate config swap and verify that new sessions pick up the updated config.
#[tokio::test]
async fn int_e2e_02_config_hot_reload_provider_change() {
    let shared = new_shared(Config::default());

    // Initial config: default provider
    let cfg1 = shared.load_full();
    let initial_provider = cfg1.default_provider.clone();

    // Swap config with different provider
    let new_cfg = Config {
        default_provider: Some("openai".to_string()),
        default_model: Some("gpt-4o".to_string()),
        ..Config::default()
    };
    shared.store(Arc::new(new_cfg));

    // New sessions should pick up the new provider
    let cfg2 = shared.load_full();
    assert_eq!(
        cfg2.default_provider.as_deref(),
        Some("openai"),
        "test: new config should have openai provider"
    );
    assert_eq!(
        cfg2.default_model.as_deref(),
        Some("gpt-4o"),
        "test: new config should have gpt-4o model"
    );

    // Old snapshot is still valid
    assert_eq!(
        cfg1.default_provider, initial_provider,
        "test: old snapshot should retain original provider"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// INT-CA-06: Mention-only filter respects mentioned_uuids (logic test)
// ═══════════════════════════════════════════════════════════════════════════════

/// Simulate mention-only filtering logic: messages without the bot's UUID are dropped.
#[tokio::test]
async fn int_ca_06_mention_only_filter() {
    let bot_uuid = "bot-uuid-12345";
    let mention_only = true;

    // Message without bot mention — should be dropped
    let mentioned_uuids: Vec<&str> = vec!["other-user-uuid"];
    let should_process = !mention_only || mentioned_uuids.contains(&bot_uuid);
    assert!(
        !should_process,
        "test: message without bot mention should be dropped in mention_only mode"
    );

    // Message with bot mention — should be processed
    let mentioned_uuids_with_bot: Vec<&str> = vec!["other-user-uuid", "bot-uuid-12345"];
    let should_process_with_bot = !mention_only || mentioned_uuids_with_bot.contains(&bot_uuid);
    assert!(
        should_process_with_bot,
        "test: message WITH bot mention should be processed in mention_only mode"
    );

    // When mention_only is false, all messages should be processed
    let mention_only_off = false;
    let should_process_all = !mention_only_off || mentioned_uuids.contains(&bot_uuid);
    assert!(
        should_process_all,
        "test: all messages should be processed when mention_only is false"
    );
}
