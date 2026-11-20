#![cfg(test)]

use super::{ApprovalRouter, ToolApprovalAnswer};
use crate::tools::ApprovalDenialCause;

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

#[test]
fn approval_router_denies_a_concurrent_tool_registration_with_its_cause() {
    let router = ApprovalRouter::new();
    let (first_tx, mut first_rx) = tokio::sync::oneshot::channel::<bool>();
    let (second_tx, mut second_rx) = tokio::sync::oneshot::channel::<ToolApprovalAnswer>();

    assert!(router.register("rewind-pending".to_string(), first_tx));
    assert!(
        !router.register_tool("call-second".to_string(), "shell".to_string(), second_tx),
        "a second registration must be refused"
    );
    assert_eq!(
        second_rx.try_recv(),
        Ok(ToolApprovalAnswer::Denied(ApprovalDenialCause::ConcurrentPending)),
        "the refused tool call learns why it was refused"
    );
    assert!(!router.is_pending("call-second"), "the refused call is not pending");

    assert!(router.resolve("rewind-pending", true));
    assert_eq!(first_rx.try_recv(), Ok(true), "first pending approval must stay intact");
}

#[test]
fn approval_router_fails_unanswered_and_switched_tool_approvals_closed_with_their_cause() {
    let router = ApprovalRouter::new();
    let (tx, mut rx) = tokio::sync::oneshot::channel::<ToolApprovalAnswer>();
    assert!(router.register_tool("call-unanswered".to_string(), "shell".to_string(), tx));
    assert!(router.resolve_unanswered("call-unanswered", ApprovalDenialCause::NoTerminal));
    assert_eq!(
        rx.try_recv(),
        Ok(ToolApprovalAnswer::Denied(ApprovalDenialCause::NoTerminal)),
        "an unanswered approval keeps its cause"
    );
    assert!(
        !router.resolve_unanswered("call-unanswered", ApprovalDenialCause::NoTerminal),
        "an unanswered approval resolves once"
    );

    let (tx, mut rx) = tokio::sync::oneshot::channel::<ToolApprovalAnswer>();
    assert!(router.register_tool("call-switched".to_string(), "shell".to_string(), tx));
    assert_eq!(router.resolve_all(false), vec!["call-switched".to_string()]);
    assert_eq!(
        rx.try_recv(),
        Ok(ToolApprovalAnswer::Denied(ApprovalDenialCause::SessionSwitched)),
        "a session switch fails the tool approval closed with its cause"
    );

    let (tx, mut rx) = tokio::sync::oneshot::channel::<bool>();
    assert!(router.register("rewind-unanswered".to_string(), tx));
    assert!(router.resolve_unanswered("rewind-unanswered", ApprovalDenialCause::NoAnswer));
    assert_eq!(rx.try_recv(), Ok(false), "a plain entry keeps its yes/no contract");
}
