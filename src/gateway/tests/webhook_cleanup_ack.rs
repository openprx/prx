#![cfg(test)]
//! A webhook turn that fails before its durable input write must answer with
//! its own failure. Every acknowledgement registered with the job monitor is
//! resolved by its owner; none is dropped unanswered.

use super::*;

/// Bound on one webhook exchange in this fixture.
const EXCHANGE_DEADLINE: Duration = Duration::from_secs(30);

/// Real gateway turn, real SQLite memory and real ingress lease acquisition:
/// the lease directory name is taken by a regular file, so ingress
/// preparation fails before any input is recorded.
#[tokio::test]
async fn ingress_preparation_failure_reports_its_cause_not_a_lost_acknowledgement() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let workspace = tempfile::tempdir().expect("test: webhook workspace");
    let mut state = webhook_test_state_at(provider, workspace.path().to_path_buf());
    install_webhook_turn_runtime(&mut state);
    std::fs::write(workspace.path().join(".prx-ingress-recovery-v1"), b"not a directory")
        .expect("test: occupy the ingress lease directory name");

    let response = tokio::time::timeout(
        EXCHANGE_DEADLINE,
        handle_webhook(
            State(state),
            test_connect_info(),
            test_uri(),
            HeaderMap::new(),
            Bytes::from_static(br#"{"message":"hello"}"#),
        ),
    )
    .await
    .expect("test: webhook exchange deadline")
    .into_response();
    let status = response.status();
    let body = response.into_body().collect().await.expect("test: body").to_bytes();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(
        !body.contains("acknowledgement was lost") && !body.contains("persistence is unconfirmed"),
        "a refusal before the input write has nothing unconfirmed: {body}"
    );
    assert_eq!(provider_impl.calls.load(Ordering::SeqCst), 0, "no turn may run");
}
