//! Called-tool token admission at dispatch. These cells build the context by
//! hand: no bindings, no turn surface and no decision context. The budget is
//! supplied directly, so a refusal can only come from the dispatch predicate.
use super::*;
use crate::tools::execution::admission::CallTokenBudget;
use std::sync::atomic::AtomicU64;

const CONFIG_RELOAD: &str = "config_reload";

fn budget_context(budget: CallTokenBudget) -> ToolExecutionContext {
    let mut context = context();
    context.call_budget = Some(budget);
    assert!(context.execution_bindings.is_none());
    assert!(context.turn_surface.is_none());
    context
}

fn fixed(remaining: u64, estimate: Option<u64>) -> CallTokenBudget {
    CallTokenBudget::new(move || remaining, move |_| estimate)
}

fn command() -> ToolExecutionCommand {
    ToolExecutionCommand::new(CONFIG_RELOAD, serde_json::json!({"value":1}))
}

/// A selected tool whose own estimate (500) exceeds the live tokens (100) is
/// refused at dispatch with the specific error and 0 backend calls.
#[tokio::test]
async fn selected_oversized_tool_is_refused_at_dispatch() {
    let fixture = fixture();
    let service = fixture.service(CONFIG_RELOAD, vec![], ToolExecutionDecision::Allow, approved(), true);
    let outcome = service
        .execute(command(), budget_context(fixed(100, Some(500))), None)
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("tool 'config_reload' estimates 500 tokens but 100 remain")),
        "unexpected diagnostic: {:?}",
        outcome.error
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

/// In-budget control, including equality (500 <= 500): exactly 1 backend call.
#[tokio::test]
async fn in_budget_tool_is_admitted_at_dispatch() {
    for remaining in [1000, 500] {
        let fixture = fixture();
        let service = fixture.service(CONFIG_RELOAD, vec![], ToolExecutionDecision::Allow, approved(), true);
        let outcome = service
            .execute(command(), budget_context(fixed(remaining, Some(500))), None)
            .await;
        assert_eq!(outcome.status, ToolExecutionStatus::Succeeded, "remaining={remaining}");
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1, "remaining={remaining}");
    }
}

/// A missing estimate (unpriced route) refuses with the specific error and 0 backend calls.
#[tokio::test]
async fn tool_without_estimate_is_refused_at_dispatch() {
    let fixture = fixture();
    let service = fixture.service(CONFIG_RELOAD, vec![], ToolExecutionDecision::Allow, approved(), true);
    let outcome = service
        .execute(command(), budget_context(fixed(1000, None)), None)
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("tool 'config_reload' has no current token estimate")),
        "unexpected diagnostic: {:?}",
        outcome.error
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

/// Live narrowing between calls: the first call fits (1 backend call); after
/// the live tokens drop to 100 the next call is refused with 0 extra backend
/// calls, and nothing resets the lowered budget.
#[tokio::test]
async fn lowered_live_tokens_refuse_the_next_dispatch_without_reset() {
    let fixture = fixture();
    let service = fixture.service(CONFIG_RELOAD, vec![], ToolExecutionDecision::Allow, approved(), true);
    let live = Arc::new(AtomicU64::new(1000));
    let reader = Arc::clone(&live);
    let budget = CallTokenBudget::new(move || reader.load(Ordering::SeqCst), |_| Some(500));
    let first = service.execute(command(), budget_context(budget.clone()), None).await;
    assert_eq!(first.status, ToolExecutionStatus::Succeeded);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);

    live.store(100, Ordering::SeqCst);
    let second = service.execute(command(), budget_context(budget), None).await;
    assert_eq!(second.status, ToolExecutionStatus::Denied);
    assert!(
        second
            .error
            .as_deref()
            .is_some_and(|error| error.contains("estimates 500 tokens but 100 remain")),
        "unexpected diagnostic: {:?}",
        second.error
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(live.load(Ordering::SeqCst), 100);
}

/// The final pre-invocation check is a second read of the live budget: the
/// admission read sees 1000 and the final read sees 100, so the call is refused
/// before the backend with 0 calls.
#[tokio::test]
async fn budget_narrowed_before_invocation_is_refused_at_final_admission() {
    let fixture = fixture();
    let service = fixture.service(CONFIG_RELOAD, vec![], ToolExecutionDecision::Allow, approved(), true);
    let reads = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&reads);
    let budget = CallTokenBudget::new(
        move || {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                1000
            } else {
                100
            }
        },
        |_| Some(500),
    );
    let outcome = service.execute(command(), budget_context(budget), None).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Denied);
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("estimates 500 tokens but 100 remain")),
        "unexpected diagnostic: {:?}",
        outcome.error
    );
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

/// The estimator receives the called tool's own serialized schema bytes.
#[tokio::test]
async fn estimator_receives_the_called_tools_schema_bytes() {
    let fixture = fixture();
    let service = fixture.service(CONFIG_RELOAD, vec![], ToolExecutionDecision::Allow, approved(), true);
    let expected = serde_json::to_vec(&service.current_descriptor(CONFIG_RELOAD).unwrap().parameters)
        .unwrap()
        .len();
    let seen = Arc::new(AtomicU64::new(u64::MAX));
    let sink = Arc::clone(&seen);
    let budget = CallTokenBudget::new(
        || 10_000,
        move |bytes| {
            sink.store(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::SeqCst);
            Some(1)
        },
    );
    let outcome = service.execute(command(), budget_context(budget), None).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(expected > 0);
    assert_eq!(seen.load(Ordering::SeqCst), u64::try_from(expected).unwrap_or(u64::MAX));
}

/// `capability_search` is a zero-token discovery action: admitted with no estimate.
#[tokio::test]
async fn capability_search_is_a_zero_token_discovery_action() {
    let fixture = fixture();
    let service = fixture.service(
        "capability_search",
        vec![],
        ToolExecutionDecision::Allow,
        approved(),
        true,
    );
    let outcome = service
        .execute(
            ToolExecutionCommand::new("capability_search", serde_json::json!({"value":1})),
            budget_context(fixed(0, None)),
            None,
        )
        .await;
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

/// A context with no budget (non-model entry points) is not token-gated: 1 backend call.
#[tokio::test]
async fn absent_budget_admits_without_token_gate() {
    let fixture = fixture();
    let service = fixture.service(CONFIG_RELOAD, vec![], ToolExecutionDecision::Allow, approved(), true);
    let context = context();
    assert!(context.call_budget.is_none());
    let outcome = service.execute(command(), context, None).await;
    assert_eq!(outcome.status, ToolExecutionStatus::Succeeded);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}
