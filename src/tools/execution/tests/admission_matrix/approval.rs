use super::*;
use crate::security::policy::AutonomyLevel;

fn autonomy_service(fixture: &Fixture, name: &'static str, autonomy: AutonomyLevel) -> ToolExecutionService {
    let mut service = fixture.service(name, vec![], ToolExecutionDecision::Ask, approved(), true);
    service.policy = Arc::new(SecurityEffectPolicy::new(Arc::new(SecurityPolicy {
        autonomy,
        ..SecurityPolicy::default()
    })));
    service.approval = Arc::new(DenyApprovalStrategy);
    service
}

/// Supervised still Asks after the binding `requires_approval` force is removed.
/// Full-mode discovered tools then follow 02a `SecurityEffectPolicy`.
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
