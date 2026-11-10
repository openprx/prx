use super::*;
use crate::security::policy::AutonomyLevel;
use crate::tools::capability::execution_binding::ExecutionBinding;
use std::collections::BTreeMap;

fn autonomy_service(fixture: &Fixture, name: &'static str, autonomy: AutonomyLevel) -> ToolExecutionService {
    let mut service = fixture.service(name, vec![], ToolExecutionDecision::Ask, approved(), true);
    service.policy = Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy {
        autonomy,
        ..SecurityPolicy::default()
    })));
    service.approval = Arc::new(DenyApprovalStrategy);
    service
}

fn issued_binding(
    fixture: &Fixture,
    name: &'static str,
    envelope: RuntimeEnvelope,
    requires_approval: bool,
) -> ExecutionBinding {
    let tool = RecordingTool {
        root_name: name,
        aliases: vec![],
        calls: Arc::clone(&fixture.calls),
        arguments: Arc::clone(&fixture.arguments),
        stages: Arc::clone(&fixture.stages),
    };
    let catalog = Arc::new(
        CapabilityCatalog::build(
            &[Box::new(tool) as Box<dyn Tool>],
            &CatalogSources::default(),
            &SurfacePolicy::default(),
        )
        .expect("test: catalog for binding pin"),
    );
    let entry = catalog
        .entry_for_public_name(name)
        .expect("test: catalog entry for binding pin");
    ExecutionBinding::issued(
        crate::causal_tree::ids::DecisionId::new_runtime(),
        entry,
        Arc::clone(&catalog),
        envelope,
        true,
    )
    .with_required_approval(requires_approval)
}

/// Supervised still Asks for a discovered tool when the bindings map is absent.
/// The `requires_approval()` Ask-force is kept until R9 (ruling R2-1).
#[tokio::test]
async fn discovered_tool_still_asks_under_supervised() {
    let fixture = fixture();
    let service = autonomy_service(&fixture, "project_tool", AutonomyLevel::Supervised);
    let context = context();
    assert!(context.execution_bindings.is_none());
    let outcome = service
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({"value": 1})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.decision, Some(ToolExecutionDecision::Ask));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn experiment_action_without_binding_refuses_with_literal_message() {
    let (workspace, fabric, chat_binding, origin) =
        crate::runtime::h2_entry::recorded_terminal_approval_fixture().await;
    let fixture = fixture();
    let service = autonomy_service(&fixture, "skills_manage", AutonomyLevel::Full);
    let mut context = context();
    context.set_h2_origin(Some(origin));
    assert!(context.execution_bindings.is_none());
    let outcome = service
        .execute(
            ToolExecutionCommand::new(
                "skills_manage",
                serde_json::json!({"value": 1, "action": "lifecycle_experiment_catalog"}),
            ),
            context,
            None,
        )
        .await;
    drop((workspace, fabric, chat_binding));
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert!(
        outcome
            .model_content
            .contains("experiment action has no exact execution binding"),
        "{}",
        outcome.model_content
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn full_autonomy_requires_approval_binding_asks() {
    let fixture = fixture();
    let service = autonomy_service(&fixture, "project_tool", AutonomyLevel::Full);
    let mut context = context();
    let binding = issued_binding(&fixture, "project_tool", context.envelope.clone(), true);
    context.execution_bindings = Some(Arc::new(BTreeMap::from([("project_tool".to_string(), binding)])));
    let outcome = service
        .execute(
            ToolExecutionCommand::new("project_tool", serde_json::json!({"value": 1})),
            context,
            None,
        )
        .await;
    assert_eq!(outcome.decision, Some(ToolExecutionDecision::Ask));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn experiment_action_with_binding_and_h2_origin_proceeds() {
    let (workspace, fabric, chat_binding, origin) =
        crate::runtime::h2_entry::recorded_terminal_approval_fixture().await;
    let workspace_id = origin.record().workspace.clone();
    let run_id = origin.record().run.clone();
    let session = origin.record().session.clone();
    let actor = origin.record().actor.clone();
    let event = origin.record().event.clone();
    let fixture = fixture();
    let service = autonomy_service(&fixture, "skills_manage", AutonomyLevel::Full);
    let mut envelope = RuntimeEnvelope::agent(&workspace_id, &run_id)
        .with_channel("terminal")
        .with_sender("local-user")
        .with_owner_id(&actor)
        .with_source_message_event_id(&event);
    envelope.session_key = session;
    let mut context = context();
    context.envelope = envelope.clone();
    context.set_h2_origin(Some(origin));
    context.execution_bindings = Some(Arc::new(BTreeMap::from([(
        "skills_manage".to_string(),
        issued_binding(&fixture, "skills_manage", envelope, false),
    )])));
    let outcome = service
        .execute(
            ToolExecutionCommand::new(
                "skills_manage",
                serde_json::json!({"value": 1, "action": "lifecycle_experiment_catalog"}),
            ),
            context,
            None,
        )
        .await;
    drop((workspace, fabric, chat_binding));
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(outcome.model_content, "executed:skills_manage");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}
