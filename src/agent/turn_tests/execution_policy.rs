#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]

use super::*;

struct PolicyProbeTool(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl Tool for PolicyProbeTool {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "PRX policy probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::tools::ToolResult {
            success: true,
            output: "allowed".to_string(),
            error: None,
        })
    }
}

#[tokio::test]
async fn configured_cli_policy_controls_tool_service_and_preserves_actor_scope() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(PolicyProbeTool(Arc::clone(&executions)))]);
    let mut policy = SecurityPolicy::default();
    policy.workspace_dir = workspace.path().to_path_buf();
    // The identity ACL is a permission limit: it applies below Full (R51).
    policy.autonomy = crate::security::AutonomyLevel::Supervised;
    policy.scope_default_allow = false;
    policy.scope_rules.push(crate::config::ScopeRule {
        user: Some("agent".into()),
        channel: Some("cli".into()),
        chat_type: Some("private".into()),
        tools_allow: vec!["memory_recall".into()],
        tools_deny: Vec::new(),
        send_allow: Vec::new(),
        send_deny: Vec::new(),
        assign_allow: Vec::new(),
        assign_deny: Vec::new(),
    });
    let build = |policy: Arc<SecurityPolicy>| {
        build_agent_tool_runtime_adapter(AgentToolRuntimeInputs {
            scope: None,
            channel_name: "cli",
            tools_registry: Arc::clone(&registry),
            approval: None,
            approval_resolver: None,
            cancellation: None,
            ledger: None,
            unscoped_policy: Some(policy),
        })
    };
    let adapter = build(Arc::new(policy.clone()));
    assert_eq!(adapter.tool_execution_context.envelope.sender.as_deref(), Some("agent"));
    assert_eq!(adapter.tool_execution_context.envelope.channel.as_deref(), Some("cli"));
    assert_eq!(adapter.tool_execution_context.chat_type, "private");
    assert_eq!(adapter.tool_execution_context.chat_id, "agent:local");
    assert!(!adapter.tool_execution_context.is_local_operator());
    let outcome = adapter
        .tool_execution_service
        .expect("tool service")
        .execute(
            crate::tools::ToolExecutionCommand::new("memory_recall", serde_json::json!({})),
            adapter.tool_execution_context,
            None,
        )
        .await;
    assert_eq!(outcome.status, crate::tools::ToolExecutionStatus::Succeeded);
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);

    policy.scope_rules.clear();
    policy.scope_rules.push(crate::config::ScopeRule {
        user: Some("owner_digest".into()),
        channel: Some("terminal".into()),
        chat_type: Some("private".into()),
        tools_allow: vec!["memory_recall".into()],
        tools_deny: Vec::new(),
        send_allow: Vec::new(),
        send_deny: Vec::new(),
        assign_allow: Vec::new(),
        assign_deny: Vec::new(),
    });
    let adapter = build(Arc::new(policy));
    let outcome = adapter
        .tool_execution_service
        .expect("tool service")
        .execute(
            crate::tools::ToolExecutionCommand::new("memory_recall", serde_json::json!({})),
            adapter.tool_execution_context,
            None,
        )
        .await;
    assert_eq!(outcome.status, crate::tools::ToolExecutionStatus::Denied);
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn scheduled_creator_scope_precedes_cli_policy_override() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let mut creator = SecurityPolicy::default();
    creator.workspace_dir = workspace.path().to_path_buf();
    creator.autonomy = crate::security::AutonomyLevel::ReadOnly;
    let creator = Arc::new(creator);
    let context = crate::tools::ToolExecutionContext::new(
        RuntimeEnvelope::cron(workspace.path().to_string_lossy(), "prx_job", "attempt")
            .with_sender("creator")
            .with_channel("telegram"),
        "private",
    )
    .with_chat_id("creator_chat");
    let scheduled = crate::tools::execution::ScheduledToolScope {
        context,
        policy: Arc::clone(&creator),
    };
    let mut cli = SecurityPolicy::default();
    cli.autonomy = crate::security::AutonomyLevel::Full;
    crate::tools::execution::with_scheduled_job_tool_scope(Some(scheduled), async {
        let (policy, context) = agent_tool_execution_context(None, "cli", Some(Arc::new(cli)));
        assert!(Arc::ptr_eq(&policy, &creator));
        assert_eq!(context.envelope.sender.as_deref(), Some("creator"));
        assert_eq!(context.envelope.channel.as_deref(), Some("telegram"));
        assert_eq!(context.chat_id, "creator_chat");
    })
    .await;
}

#[test]
fn grant_op_for_sessions_spawn_matches_tool_gate() {
    use crate::acl::approval_grant::RiskLevel;

    // spawn (explicit) → exact `sessions_spawn:spawn`, Medium.
    match grant_op_for_call("sessions_spawn", &serde_json::json!({"action": "spawn"})) {
        Some(GrantOp::ResourceExact(op, risk)) => {
            assert_eq!(op, "sessions_spawn:spawn");
            assert_eq!(risk, RiskLevel::Medium);
        }
        _ => panic!("expected exact spawn grant"),
    }

    // default (no action) also derives the spawn grant.
    match grant_op_for_call("sessions_spawn", &serde_json::json!({"task": "do it"})) {
        Some(GrantOp::ResourceExact(op, risk)) => {
            assert_eq!(op, "sessions_spawn:spawn");
            assert_eq!(risk, RiskLevel::Medium);
        }
        _ => panic!("expected exact spawn grant for default action"),
    }

    // kill → exact `sessions_spawn:kill:<run_id>`, Medium (run_id trimmed).
    match grant_op_for_call(
        "sessions_spawn",
        &serde_json::json!({"action": "kill", "run_id": "  run-1  "}),
    ) {
        Some(GrantOp::ResourceExact(op, risk)) => {
            assert_eq!(op, "sessions_spawn:kill:run-1");
            assert_eq!(risk, RiskLevel::Medium);
        }
        _ => panic!("expected exact kill grant"),
    }

    // steer → exact `sessions_spawn:steer:<run_id>`, Low.
    match grant_op_for_call(
        "sessions_spawn",
        &serde_json::json!({"action": "steer", "run_id": "run-2", "message": "hi"}),
    ) {
        Some(GrantOp::ResourceExact(op, risk)) => {
            assert_eq!(op, "sessions_spawn:steer:run-2");
            assert_eq!(risk, RiskLevel::Low);
        }
        _ => panic!("expected exact steer grant"),
    }

    // Read-only actions are not gated by the tool, so derive no grant.
    assert!(grant_op_for_call("sessions_spawn", &serde_json::json!({"action": "list"})).is_none());
    assert!(
        grant_op_for_call(
            "sessions_spawn",
            &serde_json::json!({"action": "history", "run_id": "x"})
        )
        .is_none()
    );
}

#[test]
fn is_write_tool_classifies_read_tools_as_safe() {
    for name in [
        "file_read",
        "grep",
        "web_search",
        "web_search_tool",
        "web_fetch",
        "memory_recall",
        "memory_search",
        "memory_get",
        "document_search",
        "document_get_chunk",
        "sessions_list",
        "sessions_history",
        "session_status",
        "agents_list",
        "image_info",
        "hardware_board_info",
        "hardware_memory_map",
        "hardware_memory_read",
    ] {
        assert!(
            !is_write_tool(name),
            "expected `{name}` to be classified as read-only (plan mode should let it run)"
        );
    }
}

#[test]
fn is_write_tool_classifies_mutating_tools_as_writes() {
    for name in [
        "file_write",
        "shell",
        "git_operations",
        "memory_store",
        "memory_forget",
        "memory_reindex",
        "sessions_spawn",
        "sessions_send",
        "delegate",
        "subagents",
        "cron",
        "config_reload",
        "proxy_config",
        "http_request",
        "pushover",
        "message_send",
        "composio",
        "mcp",
    ] {
        assert!(
            is_write_tool(name),
            "expected `{name}` to be classified as write (plan mode should intercept it)"
        );
    }
}

#[test]
fn unknown_tool_defaults_to_write_classification() {
    assert!(
        is_write_tool("unknown_third_party_tool_v2"),
        "unknown tools must be conservatively treated as writes"
    );
}

#[test]
fn preview_tool_arguments_truncates_long_payloads() {
    let big = serde_json::json!({"content": "x".repeat(1024)});
    let preview = preview_tool_arguments(&big);
    // truncate_with_ellipsis caps at 160 chars and appends "..."
    assert!(preview.chars().count() <= 163, "preview length unexpected: {preview}");
}

/// A human approval of `file_edit` mints the same signed, single-use,
/// principal-bound `file_edit:edit:*` grant as `file_write`: it authorizes the
/// tool's real resolved operation once and is refused on replay.
#[test]
fn file_edit_grant_authorizes_once() {
    let workspace = tempfile::tempdir().expect("test workspace");
    let target = workspace.path().join("notes.txt");
    std::fs::write(&target, "one\n").expect("test target");
    let policy = SecurityPolicy {
        autonomy: crate::security::AutonomyLevel::Supervised,
        workspace_dir: workspace.path().to_path_buf(),
        ..SecurityPolicy::default()
    };
    let scope = ScopeContext {
        policy: &policy,
        sender: "user",
        channel: "terminal",
        chat_type: "private",
        chat_id: "terminal:user",
        session_key: Some("terminal:user"),
        owner_id: None,
        owner_isolated: false,
        topic_id: None,
        task_id: None,
        source_message_event_id: None,
        config_generation_id: None,
        config_source_revision: None,
    };
    let args = serde_json::json!({"path": "notes.txt", "old_string": "one", "new_string": "two"});
    let issued = runtime_approval_grant_for_call("file_edit", &args, Some(&scope)).expect("file_edit grant");
    // The tool reads the grant from its runtime arguments with the trusted
    // scope the executor attaches, which binds the caller principal.
    let mut runtime_args = args;
    runtime_args[crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG] =
        serde_json::to_value(&issued).expect("serialize grant");
    runtime_args["_zc_scope_trusted"] = serde_json::Value::Bool(true);
    runtime_args["_zc_scope"] = serde_json::json!({
        "workspace_id": workspace.path().to_string_lossy(),
        "channel": "terminal",
        "sender": "user",
        "chat_id": "terminal:user",
    });
    let grant = ApprovalGrant::from_runtime_args("file_edit", &runtime_args).expect("runtime grant");
    let resolved = std::fs::canonicalize(&target).expect("canonical target");
    let canonical_workspace = std::fs::canonicalize(workspace.path()).expect("canonical workspace");
    let (operation, risk, launch, git) =
        crate::security::approval_operation::file_operation("file_edit", &canonical_workspace, &resolved);
    assert!(!launch && !git, "an ordinary file is a medium-risk edit");
    let gate = crate::security::SideEffectGate::new(&policy);
    gate.authorize_resource_operation("file_edit", &operation, risk, Some(&grant))
        .expect("the grant authorizes the resolved edit");
    let replay = crate::security::SideEffectGate::new(&policy)
        .authorize_resource_operation("file_edit", &operation, risk, Some(&grant))
        .expect_err("a single-use grant is refused on replay");
    assert!(
        replay.contains("approval"),
        "replay refusal names the approval: {replay}"
    );
}

struct WriteProbeTool(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl Tool for WriteProbeTool {
    fn name(&self) -> &str {
        "prx_probe_write"
    }

    fn description(&self) -> &str {
        "PRX approval probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::tools::ToolResult {
            success: true,
            output: "written".to_string(),
            error: None,
        })
    }
}

/// Answers every approval with one scripted decision and counts the asks.
struct ScriptedResolver {
    decision: ApprovalDecision,
    asked: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl ApprovalResolver for ScriptedResolver {
    async fn resolve(&self, _request: &ApprovalRequest, _channel: &str) -> ApprovalDecision {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.decision.clone()
    }
}

/// CR-2 `a`: a session grant approves this call and later calls of the same
/// tool without asking again; a single grant asks every time.
#[tokio::test]
async fn session_grant_approves_later_calls_without_asking() {
    for (decision, expected_asks) in [(ApprovalDecision::GrantForSession, 1), (ApprovalDecision::Grant, 2)] {
        let workspace = tempfile::tempdir().expect("test workspace");
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(WriteProbeTool(Arc::clone(&executions)))]);
        let mut policy = SecurityPolicy::default();
        policy.workspace_dir = workspace.path().to_path_buf();
        policy.autonomy = crate::security::AutonomyLevel::Supervised;
        let manager = Arc::new(ApprovalManager::new());
        let resolver = Arc::new(ScriptedResolver {
            decision: decision.clone(),
            asked: std::sync::atomic::AtomicUsize::new(0),
        });
        let adapter = build_agent_tool_runtime_adapter(AgentToolRuntimeInputs {
            scope: None,
            channel_name: "terminal",
            tools_registry: Arc::clone(&registry),
            approval: Some(Arc::clone(&manager)),
            approval_resolver: Some(Arc::clone(&resolver) as Arc<dyn ApprovalResolver>),
            cancellation: None,
            ledger: None,
            unscoped_policy: Some(Arc::new(policy)),
        });
        let service = adapter.tool_execution_service.expect("tool service");
        for _ in 0..2 {
            let outcome = service
                .execute(
                    crate::tools::ToolExecutionCommand::new("prx_probe_write", serde_json::json!({})),
                    adapter.tool_execution_context.clone(),
                    None,
                )
                .await;
            assert_eq!(
                outcome.status,
                crate::tools::ToolExecutionStatus::Succeeded,
                "{decision:?}"
            );
        }
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 2, "{decision:?}");
        assert_eq!(
            resolver.asked.load(std::sync::atomic::Ordering::SeqCst),
            expected_asks,
            "{decision:?}"
        );
        assert_eq!(
            manager.is_session_allowlisted("prx_probe_write"),
            matches!(decision, ApprovalDecision::GrantForSession)
        );
    }
}
