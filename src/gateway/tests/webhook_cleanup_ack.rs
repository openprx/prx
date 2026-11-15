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

/// Bound on reaching the held ingress preparation.
const PREPARATION_DEADLINE: Duration = Duration::from_secs(30);
/// Bound on the killed job reaching its terminal phase.
const JOB_TERMINAL_DEADLINE: Duration = Duration::from_secs(30);
/// Bound on the released preparation settling its own ticket.
const DRAIN_DEADLINE: Duration = Duration::from_secs(30);

/// Real async webhook job, real SQLite and real ingress preparation, held in
/// its owned task. Killing the job while the caller awaits preparation drops
/// the admission future before any input write: its owner must answer the job
/// acknowledgement as a confirmed empty cleanup, must not latch the gateway
/// cleanup owner failed, and the next turn must run.
#[tokio::test]
async fn job_killed_during_ingress_preparation_answers_its_acknowledgement() {
    let provider_impl = Arc::new(MockProvider::default());
    let provider: Arc<dyn Provider> = provider_impl.clone();
    let workspace = tempfile::tempdir().expect("test: webhook workspace");
    let mut state = webhook_test_state_at(provider, workspace.path().to_path_buf());
    install_webhook_turn_runtime(&mut state);
    let cleanup = Arc::clone(
        &state
            .turn_runtime
            .as_ref()
            .expect("test: installed turn runtime")
            .pin()
            .publication_slot
            .cleanup,
    );
    let (reached, release) = crate::memory::fabric::preparation_pause::install(&workspace.path().to_string_lossy());

    let response = tokio::time::timeout(
        EXCHANGE_DEADLINE,
        handle_webhook(
            State(state.clone()),
            test_connect_info(),
            axum::http::Uri::from_static("/webhook?mode=async"),
            HeaderMap::new(),
            Bytes::from_static(br#"{"message":"hello"}"#),
        ),
    )
    .await
    .expect("test: async submission deadline")
    .into_response();
    assert_eq!(
        response.status(),
        StatusCode::ACCEPTED,
        "async submission is acknowledged"
    );
    let body = response.into_body().collect().await.expect("test: body").to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).expect("test: json body");
    let job_id = Uuid::parse_str(body["job_id"].as_str().expect("test: job id")).expect("test: job uuid");
    let work_id = runtime::registry::WorkId::parse(body["work_id"].as_str().expect("test: work id"))
        .expect("test: work id parses");

    tokio::time::timeout(PREPARATION_DEADLINE, reached)
        .await
        .expect("test: the turn must reach ingress preparation")
        .expect("test: the preparation pause was consumed");
    runtime::registry::kill(work_id, true).await;
    let snapshot = tokio::time::timeout(JOB_TERMINAL_DEADLINE, async {
        loop {
            if let Some(snapshot) = jobs::snapshot(job_id).filter(|snapshot| snapshot.status != "running") {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("test: the killed job must reach a terminal phase");
    let error = snapshot.error.clone().unwrap_or_default();
    assert!(
        !error.contains("acknowledgement was lost") && !error.contains("persistence is unconfirmed"),
        "a turn cancelled before its input write has nothing unconfirmed: {error}"
    );
    assert_eq!(snapshot.status, "cancelled", "the killed job ends cancelled: {error}");

    // Preparation runs to completion in its own task and settles its own ticket.
    release.send(()).expect("test: release the held preparation");
    tokio::time::timeout(DRAIN_DEADLINE, async {
        while cleanup.pending_count() > 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("test: every cleanup ticket must settle");
    cleanup
        .register(CancellationToken::new())
        .expect("test: the cleanup owner must not be latched failed")
        .finish(true);

    let response = tokio::time::timeout(
        EXCHANGE_DEADLINE,
        handle_webhook(
            State(state),
            test_connect_info(),
            test_uri(),
            HeaderMap::new(),
            Bytes::from_static(br#"{"message":"hello again"}"#),
        ),
    )
    .await
    .expect("test: follow-up webhook exchange deadline")
    .into_response();
    let status = response.status();
    let body = response.into_body().collect().await.expect("test: body").to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "a later turn must run: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        provider_impl.calls.load(Ordering::SeqCst),
        1,
        "only the later turn reached the model"
    );
}
