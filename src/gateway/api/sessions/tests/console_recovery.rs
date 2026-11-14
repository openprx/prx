#![cfg(test)]
//! Console transcript recovery (PR #205 review r1 #2): a process that exits
//! after the terminal commit and before the assistant projection leaves the
//! durable reply out of `conversation_turns`; the restarted gateway's
//! `recover_console_history` restores it exactly once, on real SQLite.
use super::*;

fn count(turns: &[ConversationTurn], role: &str) -> usize {
    turns.iter().filter(|turn| turn.role == role).count()
}

/// Remove the committed reply's transcript row: the store as a process
/// leaves it when it exits between the terminal commit and the projection.
fn drop_assistant_projection(workspace: &std::path::Path, session: &str) {
    let connection = rusqlite::Connection::open(workspace.join("memory/brain.db")).expect("test: db");
    let removed = connection
        .execute(
            "DELETE FROM conversation_turns WHERE session_key = ?1 AND role = 'assistant'",
            [session],
        )
        .expect("test: drop the assistant projection");
    assert_eq!(removed, 1, "test: one committed reply was projected");
}

#[tokio::test]
async fn console_reply_interrupted_before_projection_is_restored_once_after_restart() {
    let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
        .await
        .expect("test: decision fixture");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run(async move {
            let tmp = tempfile::tempdir().expect("test: tempdir");
            let session_id = "console-recovery";
            let (state, runtime, envelope, _, proof) =
                console_probe_state(&tmp, Arc::new(CapturingProvider::default()), session_id, &endpoint).await;
            let principal = envelope.memory_principal();
            let config = state.config.pin().effective.as_ref().clone();
            assert!(
                console_recordable(&config, state.mem.as_ref()),
                "test: a recorded Console turn with a recovery repository"
            );
            let retained = Arc::clone(&runtime);
            let service = runtime
                .publication_slot
                .publish(&runtime, &state)
                .await
                .expect("test: publication")
                .clone();
            let output = run_console_message_job(
                runtime,
                state.clone(),
                "what is my key?".to_owned(),
                Vec::new(),
                ConsoleTurnSource::Message(proof),
                None,
            )
            .await
            .expect("test: the console turn completes");
            assert_eq!(output.status, axum::http::StatusCode::OK, "{}", output.body);
            let committed = state
                .mem
                .list_conversation_turns(&principal, session_id, 50, 0)
                .await
                .expect("test: committed transcript");
            let reply = committed
                .iter()
                .find(|turn| turn.role == "assistant")
                .cloned()
                .expect("test: projected reply");
            assert_eq!(reply.content, "console-runtime-ok");
            retained
                .publication_slot
                .cleanup
                .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                .await
                .expect("test: Console cleanup drains");
            service.shutdown(Instant::now() + Duration::from_secs(2)).await;
            drop((retained, service, state));

            drop_assistant_projection(tmp.path(), session_id);

            // Restart: a fresh store handle and runtime on the same workspace.
            let mut restarted = test_app_state_with_config(config, Arc::new(CapturingProvider::default()));
            restarted.mem = Arc::new(crate::memory::SqliteMemory::new(tmp.path()).expect("test: reopened sqlite"));
            let runtime = console_test_runtime(&restarted);
            let api_principal = super::super::super::ApiPrincipal::of_token("prx-test-token");
            let interrupted = restarted
                .mem
                .list_conversation_turns(&principal, session_id, 50, 0)
                .await
                .expect("test: interrupted transcript");
            assert_eq!(
                count(&interrupted, "assistant"),
                0,
                "test: the reply projection was lost"
            );
            // Twice: the projection is idempotent.
            for _ in 0..2 {
                recover_console_history(&restarted, &runtime, &api_principal, session_id)
                    .await
                    .expect("test: Console history recovery");
            }
            let recovered = restarted
                .mem
                .list_conversation_turns(&principal, session_id, 50, 0)
                .await
                .expect("test: recovered transcript");
            let replies: Vec<_> = recovered.iter().filter(|turn| turn.role == "assistant").collect();
            assert_eq!(replies.len(), 1, "the reply appears exactly once: {recovered:?}");
            assert_eq!(
                replies.first().map(|turn| turn.content.as_str()),
                Some(reply.content.as_str())
            );
            assert_eq!(
                replies.first().and_then(|turn| turn.message_id.clone()),
                reply.message_id,
                "the restored row projects the committed assistant event"
            );
            assert_eq!(
                count(&recovered, "user"),
                count(&committed, "user"),
                "the input is not projected twice"
            );
            let service = runtime
                .publication_slot
                .publish(&runtime, &restarted)
                .await
                .expect("test: restarted publication")
                .clone();
            service.shutdown(Instant::now() + Duration::from_secs(2)).await;
            let retirement = Arc::clone(&runtime.publication_slot.cleanup);
            drop(runtime);
            retirement
                .drain(tokio::time::Instant::now() + Duration::from_secs(5))
                .await
                .expect("test: restarted runtime retirement drains");
        })
        .await;
}
