//! Call-budget stamps (03 R6a/R6b) and ModelTurn Deny (F4).
use super::*;
use crate::tools::execution::admission::CallTokenBudget;

const CONFIG_RELOAD: &str = "config_reload";

fn command() -> ToolExecutionCommand {
    ToolExecutionCommand::new(CONFIG_RELOAD, serde_json::json!({"value": 1}))
}

fn model_turn(mut context: ToolExecutionContext) -> ToolExecutionContext {
    context.dispatch_origin = crate::tools::DispatchOrigin::ModelTurn;
    context
}

fn operator_turn(mut context: ToolExecutionContext) -> ToolExecutionContext {
    context.dispatch_origin = crate::tools::DispatchOrigin::Operator;
    context
}

#[tokio::test]
async fn segment_context_carries_budget() {
    let child = crate::agent::turn_tests::engine_switch::admit_bound_child_segment().await;
    assert!(
        child.turn.adapter().tool_execution_context.call_budget.is_some(),
        "segment admission stamps a call budget"
    );
    child.shutdown().await;
}

#[tokio::test]
async fn model_turn_without_call_budget_is_denied() {
    let fixture = fixture();
    let service = fixture.service(CONFIG_RELOAD, vec![], ToolExecutionDecision::Allow, approved(), true);
    let outcome = service.execute(command(), model_turn(context()), None).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert_eq!(
        outcome.error.as_deref(),
        Some("tool 'config_reload' has no call budget")
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn model_turn_budget_without_spend_is_denied() {
    let fixture = fixture();
    let service = fixture.service(CONFIG_RELOAD, vec![], ToolExecutionDecision::Allow, approved(), true);
    let mut context = model_turn(context());
    context.call_budget = Some(CallTokenBudget::new(|| 4096, |_| Some(1)));
    let outcome = service.execute(command(), context, None).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert_eq!(
        outcome.error.as_deref(),
        Some("tool 'config_reload' has no call budget")
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pre_invoke_recheck_denies_missing_budget() {
    let fixture = fixture();
    let service = fixture.service(CONFIG_RELOAD, vec![], ToolExecutionDecision::Allow, approved(), true);
    let reads = Arc::new(AtomicUsize::new(0));
    let live_reads = Arc::clone(&reads);
    let mut context = model_turn(context());
    context.turn_surface = Some(Arc::new(crate::tools::execution::admission::TurnSurface::build_from(
        &service,
        service.current_descriptors(),
        &crate::config::ToolTieringConfig::default(),
        "prx_model",
        false,
        |_| true,
    )));
    assert!(
        context
            .turn_surface
            .as_ref()
            .is_some_and(|surface| surface.contains(CONFIG_RELOAD)),
        "test: admitted turn surface includes the called tool"
    );
    context.call_budget = Some(CallTokenBudget::new(|| 4096, |_| Some(1)).with_spend(
        move || (live_reads.fetch_add(1, Ordering::SeqCst) == 0).then_some(1_000),
        |_| Some(1),
    ));
    let outcome = service.execute(command(), context, None).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied, "test: recheck denies");
    assert_eq!(
        outcome.error.as_deref(),
        Some("tool 'config_reload' has no live spend ceiling"),
        "test: denial comes from the live pre-invoke recheck"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 2, "test: admit then recheck");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0, "test: backend never runs");
}

#[tokio::test]
async fn operator_dispatch_without_call_budget_is_unchanged() {
    let fixture = fixture();
    let service = fixture.service(CONFIG_RELOAD, vec![], ToolExecutionDecision::Allow, approved(), true);
    let outcome = service.execute(command(), operator_turn(context()), None).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}
