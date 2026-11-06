use super::*;
use crate::security::approval_ledger::keys::ApprovalAuthority;

fn non_read_only_calls() -> [(&'static str, serde_json::Value); 6] {
    [
        (
            "file_write",
            serde_json::json!({"value":1,"path":"project.txt","content":"project content"}),
        ),
        ("shell", serde_json::json!({"value":1,"command":"touch project.txt"})),
        (
            "message_send",
            serde_json::json!({"value":1,"channel":"prx_channel","target":"prx_target","message":"project message"}),
        ),
        ("config_reload", serde_json::json!({"value":1})),
        (
            "mcp_call",
            serde_json::json!({"value":1,"server":"prx_server","tool":"prx_action","arguments":{}}),
        ),
        ("sessions_spawn", serde_json::json!({"value":1,"task":"project task"})),
    ]
}

fn ledger_port(context: &ToolExecutionContext) -> ApprovalLedgerPort<'_> {
    let ledger = context.approval_ledger.as_ref().unwrap();
    ApprovalLedgerPort {
        memory: &ledger.memory,
        state_key: &ledger.state_key,
        workspace: &ledger.workspace,
        keyring: crate::acl::approval_grant::WitnessKeyring::global_for_tests(),
    }
}

async fn assert_no_pending(context: &ToolExecutionContext) {
    assert!(
        ledger_port(context)
            .list_pending_calls(context.envelope.owner_id.as_deref().unwrap(), 0, 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

fn autonomy_service(fixture: &Fixture, name: &'static str, autonomy: AutonomyLevel) -> ToolExecutionService {
    let mut service = fixture.service(name, vec![], ToolExecutionDecision::Ask, approved(), true);
    service.policy = Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy {
        autonomy,
        ..SecurityPolicy::default()
    })));
    // A mistaken Ask must fail closed, rather than silently using a fixture approval.
    service.approval = Arc::new(DenyApprovalStrategy);
    service
}

#[tokio::test]
async fn supervised_ask_without_resolver_is_denied_not_executed() {
    let fixture = fixture();
    let service = autonomy_service(&fixture, "config_reload", AutonomyLevel::Supervised);
    let context = context();
    assert!(context.approval_ledger.is_none());
    assert!(context.execution_bindings.is_none());
    let outcome = service.execute(command(1), context, None).await;
    assert_eq!(outcome.decision, Some(ToolExecutionDecision::Ask));
    assert_eq!(outcome.status, ToolExecutionStatus::ApprovalDenied);
    assert!(outcome.model_content.contains("no approval resolver"));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn child_call_ask_binds_to_child_context() {
    let path = tempfile::tempdir().unwrap();
    let parent = ledger_context(path.path()).await;
    let mut child = parent.clone();
    child.envelope.parent_run_id = parent.envelope.run_id.clone();
    child.envelope.run_id = Some(uuid::Uuid::new_v4().to_string());
    child.envelope.owner_id = Some("prx_child_owner".into());
    child.envelope.session_key = "prx_child_session".into();
    let ledger = child.approval_ledger.as_ref().unwrap();
    let source = crate::memory::MemoryFabric::new(Arc::clone(&ledger.memory), &ledger.workspace)
        .record_inbound_user_message(child.envelope.message_scope(), "project child call", None, None)
        .await
        .unwrap();
    child.envelope.source_message_event_id = Some(source.event_id.clone());
    assert!(child.execution_bindings.is_none());
    let fixture = fixture();
    let service = autonomy_service(&fixture, "config_reload", AutonomyLevel::Supervised);
    assert_eq!(
        service.execute(command(1), child.clone(), None).await.status,
        ToolExecutionStatus::AwaitingApproval
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_no_pending(&parent).await;
    let port = ledger_port(&child);
    let pending = port.list_pending_calls("prx_child_owner", 0, 10).await.unwrap();
    assert_eq!(pending.items.len(), 1);
    let pending = pending.items.first().unwrap();
    assert_eq!(pending.authority().run_id, child.envelope.run_id.as_deref().unwrap());
    assert_ne!(
        Some(pending.authority().run_id.as_str()),
        parent.envelope.run_id.as_deref()
    );
    assert_eq!(pending.subject().owner_id, "prx_child_owner");
    assert_eq!(pending.subject().session_key.as_deref(), Some("prx_child_session"));
    assert_eq!(pending.source_event_id(), source.event_id);
    assert!(
        port.review(
            parent.envelope.owner_id.as_deref().unwrap(),
            &parent.envelope.session_key,
            pending.authority(),
            true
        )
        .await
        .is_err()
    );
    review(&child, true).await;
    assert!(
        port.claim_if_approved(
            parent.envelope.owner_id.as_deref().unwrap(),
            &parent.envelope.session_key,
            pending.authority()
        )
        .await
        .is_err()
    );
    assert_eq!(
        service.execute(command(1), child.clone(), None).await.status,
        ToolExecutionStatus::Succeeded
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let ledger = child.approval_ledger.as_ref().unwrap();
    let resolution = ledger
        .memory
        .find_message_event_by_idempotency_key(&ledger.workspace, &pending.authority().resolution_key(""))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolution.run_id, child.envelope.run_id);
    assert_eq!(resolution.owner_id, child.envelope.owner_id);
    assert_eq!(
        service.execute(command(1), child.clone(), None).await.status,
        ToolExecutionStatus::ApprovalDenied
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn full_mode_executes_non_read_only_without_approval() {
    let path = tempfile::tempdir().unwrap();
    let context = ledger_context(path.path()).await;
    assert!(context.execution_bindings.is_none());
    for (name, args) in non_read_only_calls() {
        let fixture = fixture();
        let service = autonomy_service(&fixture, name, AutonomyLevel::Full);
        let outcome = service
            .execute(ToolExecutionCommand::new(name, args), context.clone(), None)
            .await;
        assert_eq!(
            outcome.status,
            ToolExecutionStatus::Succeeded,
            "{name}: {}",
            outcome.model_content
        );
        assert_eq!(outcome.decision, Some(ToolExecutionDecision::Allow), "{name}");
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1, "{name}");
        assert_no_pending(&context).await;
    }
}

#[tokio::test]
async fn explicit_grant_call_asks_in_full_mode() {
    let path = tempfile::tempdir().unwrap();
    let mut context = ledger_context(path.path()).await;
    assert!(context.execution_bindings.is_none());
    let fixture = fixture();
    let tool = RecordingTool {
        root_name: "skills_manage",
        aliases: vec![],
        calls: Arc::clone(&fixture.calls),
        arguments: Arc::clone(&fixture.arguments),
        stages: Arc::clone(&fixture.stages),
    };
    context.approval_ledger.as_mut().unwrap().catalog = Arc::new(
        CapabilityCatalog::build(
            &[Box::new(tool) as Box<dyn Tool>],
            &CatalogSources::default(),
            &SurfacePolicy::default(),
        )
        .unwrap(),
    );
    let service = autonomy_service(&fixture, "skills_manage", AutonomyLevel::Full);
    let command = || {
        ToolExecutionCommand::new(
            "skills_manage",
            serde_json::json!({"value":1,"action":"lifecycle_publish","proposal_id":"prx_proposal"}),
        )
    };
    let outcome = service.execute(command(), context.clone(), None).await;
    assert_eq!(outcome.decision, Some(ToolExecutionDecision::Ask));
    assert_eq!(outcome.status, ToolExecutionStatus::AwaitingApproval);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        ledger_port(&context)
            .list_pending_calls(context.envelope.owner_id.as_deref().unwrap(), 0, 10)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    review(&context, true).await;
    assert_eq!(
        service.execute(command(), context.clone(), None).await.status,
        ToolExecutionStatus::Succeeded
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn policy_deny_refused_without_backend_call() {
    let path = tempfile::tempdir().unwrap();
    let context = ledger_context(path.path()).await;
    assert!(context.execution_bindings.is_none());
    for (name, args) in [
        (
            "file_write",
            serde_json::json!({"value":1,"path":"project.txt","content":"project content"}),
        ),
        ("shell", serde_json::json!({"value":1,"command":"touch project.txt"})),
    ] {
        let fixture = fixture();
        let mut service = autonomy_service(&fixture, name, AutonomyLevel::Full);
        // Path/command enforcement is not provable at the service layer; this row covers scope Deny.
        service.policy = Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            scope_rules: vec![crate::config::ScopeRule {
                tools_deny: vec![name.into()],
                ..crate::config::ScopeRule::default()
            }],
            ..SecurityPolicy::default()
        })));
        let outcome = service
            .execute(ToolExecutionCommand::new(name, args), context.clone(), None)
            .await;
        assert_eq!(
            outcome.status,
            ToolExecutionStatus::Denied,
            "{name}: {}",
            outcome.model_content
        );
        assert_eq!(outcome.decision, Some(ToolExecutionDecision::Deny), "{name}");
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0, "{name}");
        assert_no_pending(&context).await;
    }
}

#[tokio::test]
async fn readonly_autonomy_denies_every_non_read_only_class() {
    let path = tempfile::tempdir().unwrap();
    let context = ledger_context(path.path()).await;
    assert!(context.execution_bindings.is_none());
    for (name, args) in non_read_only_calls() {
        let fixture = fixture();
        let service = autonomy_service(&fixture, name, AutonomyLevel::ReadOnly);
        let outcome = service
            .execute(ToolExecutionCommand::new(name, args), context.clone(), None)
            .await;
        assert_eq!(
            outcome.status,
            ToolExecutionStatus::Denied,
            "{name}: {}",
            outcome.model_content
        );
        assert_eq!(outcome.decision, Some(ToolExecutionDecision::Deny), "{name}");
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0, "{name}");
        assert_no_pending(&context).await;
    }
}
