//! Call-budget stamps on ungated child segments (03 R6a). R6b adds the Deny cells.

#[tokio::test]
async fn segment_context_carries_budget() {
    let child = crate::agent::turn_tests::engine_switch::admit_bound_child_segment().await;
    assert!(
        child.turn.adapter().tool_execution_context.call_budget.is_some(),
        "segment admission stamps a call budget"
    );
    child.shutdown().await;
}
