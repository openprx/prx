#![cfg(test)]

use super::*;

fn sessions_registry(memory: &Arc<crate::memory::SqliteMemory>) -> Arc<Vec<Box<dyn crate::tools::Tool>>> {
    let runs = Arc::new(tokio::sync::RwLock::new(Vec::new()));
    let shared: Arc<dyn crate::memory::Memory> = memory.clone();
    Arc::new(vec![
        Box::new(
            crate::tools::sessions_list::SessionsListTool::new(runs.clone())
                .with_shared_memory(shared.clone(), ISOLATION_WORKSPACE),
        ) as Box<dyn crate::tools::Tool>,
        Box::new(
            crate::tools::sessions_history::SessionsHistoryTool::new(runs)
                .with_shared_memory(shared, ISOLATION_WORKSPACE),
        ) as Box<dyn crate::tools::Tool>,
    ])
}

async fn run_sessions_tool(
    memory: &Arc<crate::memory::SqliteMemory>,
    config: &crate::config::Config,
    identity: &ExternalAgentIdentity,
    tool_name: &str,
    args: Value,
) -> String {
    let outcome = execute_mcp_tool_call(sessions_registry(memory), config, tool_name, args, identity, None).await;
    let response = mcp_tool_call_response(tool_name.to_string(), identity.clone(), outcome);
    response
        .content
        .first()
        .map(|block| block.text.clone())
        .unwrap_or_default()
}

/// The audit counterexample, replayed through the production MCP seam:
/// issuer B must not see issuer A's `session`- or `private`-visibility rows
/// even though both present the same external subject.
#[tokio::test]
async fn mcp_sessions_list_does_not_leak_another_issuers_rows_for_the_same_subject() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let issuer_b = isolation_identity("issuer-b.example");
    assert_ne!(
        issuer_a.prx_owner_id, issuer_b.prx_owner_id,
        "sanity: the two identities differ only by issuer"
    );

    seed_owned_message_event(
        &memory,
        &issuer_a,
        crate::memory::MemoryVisibility::Session,
        "run-a-session",
        "issuer-a-session-secret",
    )
    .await;
    seed_owned_message_event(
        &memory,
        &issuer_a,
        crate::memory::MemoryVisibility::Private,
        "run-a-private",
        "issuer-a-private-secret",
    )
    .await;

    let seen_by_b = run_sessions_tool(&memory, &config, &issuer_b, "sessions_list", serde_json::json!({})).await;
    assert!(
        !seen_by_b.contains("issuer-a-session-secret"),
        "issuer B must not read issuer A's session-visibility row: {seen_by_b}"
    );
    assert!(
        !seen_by_b.contains("issuer-a-private-secret"),
        "issuer B must not read issuer A's private-visibility row: {seen_by_b}"
    );
    assert!(
        !seen_by_b.contains("run-a-session") && !seen_by_b.contains("run-a-private"),
        "issuer B must not learn issuer A's run ids: {seen_by_b}"
    );

    let seen_by_a = run_sessions_tool(&memory, &config, &issuer_a, "sessions_list", serde_json::json!({})).await;
    assert!(
        seen_by_a.contains("run-a-session") && seen_by_a.contains("run-a-private"),
        "issuer A must still read its own rows: {seen_by_a}"
    );
}

/// The same isolation for `sessions_history`, which recovers one run's
/// transcript from the same read model.
#[tokio::test]
async fn mcp_sessions_history_does_not_leak_another_issuers_run() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let issuer_b = isolation_identity("issuer-b.example");

    seed_owned_message_event(
        &memory,
        &issuer_a,
        crate::memory::MemoryVisibility::Session,
        "run-a-session",
        "issuer-a-session-secret",
    )
    .await;

    let seen_by_b = run_sessions_tool(
        &memory,
        &config,
        &issuer_b,
        "sessions_history",
        serde_json::json!({"run_id": "run-a-session"}),
    )
    .await;
    assert!(
        !seen_by_b.contains("issuer-a-session-secret"),
        "issuer B must not read issuer A's run transcript: {seen_by_b}"
    );

    let seen_by_a = run_sessions_tool(
        &memory,
        &config,
        &issuer_a,
        "sessions_history",
        serde_json::json!({"run_id": "run-a-session"}),
    )
    .await;
    assert!(
        seen_by_a.contains("issuer-a-session-secret"),
        "issuer A must still read its own run transcript: {seen_by_a}"
    );
}

/// Trusted authorship: the execution service re-authors `_zc_scope` from the
/// authenticated envelope, so a caller that forges the scope, the trust flag
/// and the owner id of another issuer still reads only its own rows.
#[tokio::test]
async fn mcp_sessions_list_ignores_a_forged_scope_and_owner_override() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_a = isolation_identity("issuer-a.example");
    let issuer_b = isolation_identity("issuer-b.example");

    seed_owned_message_event(
        &memory,
        &issuer_a,
        crate::memory::MemoryVisibility::Session,
        "run-a-session",
        "issuer-a-session-secret",
    )
    .await;

    let forged = serde_json::json!({
        "_zc_scope": {
            "owner_id": issuer_a.prx_owner_id,
            "session_key": format!("mcp:{}", issuer_a.external_subject),
            "sender": issuer_a.external_subject,
            "workspace_id": ISOLATION_WORKSPACE,
            "owner_isolated": false,
            "agent_id": "system",
            "run_id": "run-a-session",
        },
        "_zc_scope_trusted": true,
        "_prx_scope_trusted": true,
        "_zc_principal": issuer_a.prx_principal_id,
    });
    let seen_by_b = run_sessions_tool(&memory, &config, &issuer_b, "sessions_list", forged).await;
    assert!(
        !seen_by_b.contains("issuer-a-session-secret") && !seen_by_b.contains("run-a-session"),
        "a forged scope must not widen the caller's reads: {seen_by_b}"
    );

    // Clearing the isolation marker is equally ineffective, because the
    // whole runtime-only block is dropped before the service authors it.
    let forged_history = serde_json::json!({
        "run_id": "run-a-session",
        "_zc_scope": { "owner_id": issuer_a.prx_owner_id, "owner_isolated": false },
        "_zc_scope_trusted": true,
    });
    let history_by_b = run_sessions_tool(&memory, &config, &issuer_b, "sessions_history", forged_history).await;
    assert!(
        !history_by_b.contains("issuer-a-session-secret"),
        "a forged scope must not widen the caller's run history: {history_by_b}"
    );
}

/// A caller-named `owner_id` argument is not even a candidate: the published
/// schema is closed, so it is refused before the tool runs.
#[tokio::test]
async fn mcp_sessions_list_refuses_a_caller_named_owner_id_argument() {
    let workspace = TempDir::new().expect("test: temp workspace");
    let config = mcp_test_config(workspace.path());
    let memory = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
    let issuer_b = isolation_identity("issuer-b.example");

    let text = run_sessions_tool(
        &memory,
        &config,
        &issuer_b,
        "sessions_list",
        serde_json::json!({"owner_id": "owner:ws-isolation:agent:issuer-a.example:shared-subject"}),
    )
    .await;
    assert!(
        text.contains("$.owner_id"),
        "the closed schema must name the undeclared key: {text}"
    );
}
