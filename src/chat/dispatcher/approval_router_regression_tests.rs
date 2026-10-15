#![cfg(test)]

use super::ApprovalRouter;

#[test]
fn approval_router_rejects_concurrent_registration_without_replacing_first() {
    let router = ApprovalRouter::new();
    let (first_tx, mut first_rx) = tokio::sync::oneshot::channel::<bool>();
    let (second_tx, mut second_rx) = tokio::sync::oneshot::channel::<bool>();

    assert!(router.register("first".to_string(), first_tx));
    assert!(!router.register("second".to_string(), second_tx));
    assert!(router.has_pending());
    assert_eq!(second_rx.try_recv(), Ok(false), "second approval must fail closed");

    assert!(router.resolve("first", true));
    assert_eq!(first_rx.try_recv(), Ok(true), "first pending approval must stay intact");
    assert!(!router.has_pending());
}
