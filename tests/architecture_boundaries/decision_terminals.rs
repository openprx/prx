//! Selection authority and abstention contracts independent of prompt wording.
use super::helpers::{function_body, read_source, repository_root};

#[test]
pub(super) fn recovery_cannot_publish_an_executable_tool() {
    let source = read_source(&repository_root().join("src/causal_tree/directive.rs"));
    let body = function_body(&source, "directive_from_selection");
    assert!(body.contains("matches!(decision.selection, Selection::Recover { .. })"));
    assert!(body.contains("return Err(DirectiveError::ActionMismatch)"));
    let selector = read_source(&repository_root().join("src/causal_tree/decision_select.rs"));
    let select = function_body(&selector, "select");
    assert!(select.contains("top_score.execution_utility >= threshold"));
    assert!(select.contains("margin >= policy.minimum_top_margin"));
    assert!(select.contains("let evidence_ok = !insufficient || exact_read"));
    assert!(select.contains("prediction.action_confidence >= policy.minimum_choice_confidence"));
    assert!(select.contains("prediction.target_confidence >= policy.minimum_choice_confidence"));
    for required in [
        "&& enough_choice_confidence",
        "&& evidence_ok",
        "&& fresh_ok",
        "&& !target_missing",
    ] {
        assert!(select.contains(required), "missing execution condition: {required}");
    }
    assert!(!select.contains("|| exact_current_action"));
    assert!(!select.contains("pure_textual_reply"));
    assert!(!select.contains("concrete_artifact"));
}

#[test]
pub(super) fn execution_guard_is_unconditional_and_decision_clock_has_one_owner() {
    let source = read_source(&repository_root().join("src/agent/turn_engine.rs"));
    assert!(source.contains("execution_requirement.is_none(),"));
    assert!(!source.contains("if !textual_reply"));
    assert!(!source.contains("decision_select::pure_textual_reply"));
    let engine = read_source(&repository_root().join("src/causal_tree/decision_engine.rs"));
    assert!(!engine.contains("DECISION_CLOCK"));
    assert!(engine.contains("super::logical_clock::now()"));
    assert!(engine.contains("scored_count == admitted.iter().filter"));
}

#[test]
pub(super) fn confidence_terminals_include_answer_and_cli_clarification() {
    let source = read_source(&repository_root().join("src/causal_tree/decision_select.rs"));
    let terminal = function_body(&source, "terminal_abstain");
    let compact: String = terminal.split_whitespace().collect();
    assert!(compact.contains("&[CausalActionKind::Answer,CausalActionKind::Clarify,CausalActionKind::Refuse,]"));
    let generator = read_source(&repository_root().join("src/causal_tree/candidate_generator.rs"));
    assert!(!generator.contains("delivery.interactive && state.trusted.parts().delivery.can_ask_question"));
    let outcome = read_source(&repository_root().join("src/causal_tree/outcome.rs"));
    assert!(outcome.contains("answer_on_abstention"));
    let runtime = read_source(&repository_root().join("src/runtime/turn_decision.rs"));
    let publication = function_body(&runtime, "direct_answer_refs");
    assert!(publication.contains("Selection::Abstain"));
    assert!(publication.contains("action: CausalActionKind::Answer"));
    assert!(publication.contains("return BTreeSet::new()"));
    assert!(publication.contains("if fallback_answer || !gates.model_can_call_tools"));
    assert!(publication.contains("matches!(publication, GateOutcome::Blocked"));
}
