//! Persistent initialization and interactive setup.
pub mod wizard;

/// Publish the successfully committed configuration as the active workspace.
pub async fn select_workspace(target: &std::path::Path) -> anyhow::Result<()> {
    crate::config::schema::persist_active_workspace_config_dir(target).await
}
