#[cfg(feature = "llm-router")]
#[tokio::test]
async fn router_read_scope_refuses_foreign_session() -> anyhow::Result<()> {
    use super::{RouterHistoryDomain, RouterReadScope};
    use std::sync::Arc;
    let workspace = tempfile::tempdir()?;
    let config = crate::config::Config {
        workspace_dir: workspace.path().to_owned(),
        ..Default::default()
    };
    let generation = crate::config::new_shared(config).pin();
    let memory: Arc<dyn crate::memory::Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path())?);
    let proof = crate::agent::entry::test_recorded_cli_agent_turn_proof_with_selection(
        &generation,
        memory,
        "router input",
        "prx_run",
        ("ollama", "prx-local-model"),
    )
    .await;
    let domain = RouterHistoryDomain {
        workspace: proof.workspace_id().to_owned(),
        owner: proof.owner_id().to_owned(),
        session: proof.session_key().to_owned(),
        policy_sha256: [0; 32],
        embedding_model: "prx_embedding".to_owned(),
        dimensions: 1,
        embedding_version: 1,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    assert!(RouterReadScope::from_source(&proof, domain.clone(), deadline).is_ok());
    for field in ["session", "owner", "workspace"] {
        let mut foreign = domain.clone();
        match field {
            "session" => foreign.session = "foreign_session".to_owned(),
            "owner" => foreign.owner = "foreign_owner".to_owned(),
            _ => foreign.workspace = "foreign_workspace".to_owned(),
        }
        assert!(RouterReadScope::from_source(&proof, foreign, deadline).is_err());
    }
    Ok(())
}
