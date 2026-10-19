#![cfg(test)]

use crate::llm::route_decision::{ExecutionStatus, ProviderExecutionOutcome, RouteDecision};

/// Contract relied upon by the chat failure path (FIX-P1-15 / #27) and the
/// channels failure path (#21): a failed turn must produce a
/// `ProviderExecutionOutcome` that (a) carries the same `decision_id` as the
/// originating `RouteDecision` so the timeline join still works, (b) reports
/// an `AllFailed` execution status, and (c) records exactly one failed
/// attempt. Both failure paths build this outcome via `failed_for_decision`
/// before recording `provider.final_outcome` events.
#[test]
fn failed_for_decision_outcome_is_recordable_for_failed_turns() {
    let decision = RouteDecision::single_candidate("test-provider", "test-model");
    let started_at = chrono::Utc::now();
    let error = anyhow::anyhow!("simulated provider timeout");

    let outcome = ProviderExecutionOutcome::failed_for_decision(&decision, started_at, &error);

    assert_eq!(
        outcome.decision_id, decision.decision_id,
        "failed outcome must preserve the route decision_id for timeline joins"
    );
    assert!(
        matches!(outcome.status, ExecutionStatus::AllFailed { .. }),
        "a failed turn must surface as ExecutionStatus::AllFailed"
    );
    assert_eq!(
        outcome.attempts.len(),
        1,
        "failed_for_decision should record exactly one failed attempt"
    );
    assert_eq!(outcome.final_provider, "test-provider");
    assert_eq!(outcome.final_model, "test-model");
}
