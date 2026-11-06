use crate::tools::execution::tests::admission_matrix::{assert_existing_strategy, ledger_context};
async fn foreground_strategy(
    build: fn(crate::runtime::envelope::RuntimeEnvelope) -> crate::tools::ToolExecutionContext,
) {
    let path = tempfile::tempdir().unwrap();
    let mut recorded = ledger_context(path.path()).await;
    let observer = recorded.approval_ledger.take().unwrap();
    let context = build(recorded.envelope);
    assert!(context.approval_ledger.is_none());
    assert_existing_strategy(context, observer).await;
}
#[tokio::test]
async fn foreground_tui_preserves_existing_approval_without_pending() {
    foreground_strategy(super::super::approval_context::tui).await;
}
#[tokio::test]
async fn line_mode_preserves_existing_approval_without_pending() {
    foreground_strategy(super::super::approval_context::line_mode).await;
}
