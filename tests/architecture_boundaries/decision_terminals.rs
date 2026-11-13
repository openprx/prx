//! Execution guard contract independent of prompt wording.
//!
//! The legacy selector and its recovery/abstention terminals were retired
//! with the prediction hop (R9); their scanners went with them.
use super::helpers::{read_source, repository_root};

#[test]
pub(super) fn execution_guard_is_unconditional_and_decision_clock_has_one_owner() {
    let source = read_source(&repository_root().join("src/agent/turn_engine.rs"));
    assert!(source.contains("execution_requirement.is_none(),"));
    assert!(!source.contains("if !textual_reply"));
    assert!(!source.contains("decision_select::pure_textual_reply"));
}
