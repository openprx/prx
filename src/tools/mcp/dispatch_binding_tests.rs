#![cfg(test)]
//! Transport-free checks of the real MCP dispatch guard after publication drift.
use super::*;
use crate::causal_tree::ids::DecisionId;
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::capability::catalog::{CapabilityCatalog, CatalogSources, SurfacePolicy};
use crate::tools::capability::execution_binding::{ExecutionBinding, ToolExecutionError, scope_dispatch_binding};
use anyhow::Context;

const ALIAS: &str = "mcp__prx_server__read";

fn fixture() -> anyhow::Result<(tempfile::TempDir, Arc<McpTool>, ExecutionBinding)> {
    let workspace = tempfile::tempdir().context("MCP dispatch binding fixture invariant")?;
    let mut config = crate::config::Config::default();
    config.autonomy.level = crate::security::AutonomyLevel::Full;
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, workspace.path()));
    let mut mcp = McpConfig::default();
    // No command: even a broken refusal cannot start a subprocess or network.
    mcp.servers.insert("prx_server".into(), McpServerConfig::default());
    let tool = Arc::new(McpTool::new(security, mcp, workspace.path().to_owned()));
    {
        let mut state = tool.state.write();
        state.initialized = true;
        state.mcp_state_epoch = 1;
        state.discovered_tools.insert(
            "prx_server".into(),
            HashMap::from([(
                "read".into(),
                DiscoveredToolMeta {
                    description: Some("PRX read capability".into()),
                    input_schema: Some(json!({"type":"object","properties":{}})),
                },
            )]),
        );
    }
    tool.rebuild_catalog_snapshot();
    let registry = vec![crate::tools::ArcDelegatingTool::boxed(
        Arc::clone(&tool) as Arc<dyn Tool>
    )];
    let catalog = Arc::new(
        CapabilityCatalog::build(
            &registry,
            &CatalogSources {
                mcp: tool.capability_snapshot(),
                ..CatalogSources::default()
            },
            &SurfacePolicy::default(),
        )
        .context("MCP dispatch binding fixture invariant")?,
    );
    let entry = catalog
        .entry_for_public_name(ALIAS)
        .context("MCP dispatch binding fixture invariant")?;
    let binding = ExecutionBinding::issued(
        DecisionId::new_runtime(),
        entry,
        Arc::clone(&catalog),
        RuntimeEnvelope::agent("prx_workspace", "prx_run"),
        true,
    );
    Ok((workspace, tool, binding))
}

#[tokio::test]
async fn pinned_mcp_dispatch_rejects_same_schema_destination_and_epoch_drift_before_transport() -> anyhow::Result<()> {
    for epoch_only in [false, true] {
        let (_workspace, tool, binding) = fixture()?;
        let before = tool
            .shared_catalog_snapshot()
            .context("MCP dispatch binding fixture invariant")?;
        {
            let mut state = tool.state.write();
            state.mcp_state_epoch += 1;
            if !epoch_only {
                state
                    .effective_config
                    .servers
                    .get_mut("prx_server")
                    .context("MCP dispatch binding fixture invariant")?
                    .args
                    .push("prx_target_b".into());
            }
        }
        tool.rebuild_catalog_snapshot();
        let after = tool
            .shared_catalog_snapshot()
            .context("MCP dispatch binding fixture invariant")?;
        assert_eq!(
            before
                .entry(ALIAS)
                .context("MCP dispatch binding fixture invariant")?
                .fingerprint(),
            after
                .entry(ALIAS)
                .context("MCP dispatch binding fixture invariant")?
                .fingerprint()
        );
        assert_ne!(
            before
                .entry(ALIAS)
                .context("MCP dispatch binding fixture invariant")?
                .binding_digest(),
            after
                .entry(ALIAS)
                .context("MCP dispatch binding fixture invariant")?
                .binding_digest()
        );
        let error = scope_dispatch_binding(
            Some(&binding),
            tool.validate_and_call(ALIAS, "prx_server".into(), "read".into(), None),
        )
        .await
        .err()
        .context("guard or command-free transport precondition must refuse")?;
        assert_eq!(
            error.downcast_ref::<ToolExecutionError>(),
            Some(&ToolExecutionError::StaleCapability)
        );
        assert!(tool.stdio_sessions.lock().is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn pinned_mcp_dispatch_accepts_same_snapshot_and_rejects_missing_or_changed_schema() -> anyhow::Result<()> {
    let (_workspace, tool, binding) = fixture()?;
    // The unchanged guard passes into the real transport precondition, which
    // safely refuses our intentionally command-free fixture before spawning.
    let error = scope_dispatch_binding(
        Some(&binding),
        tool.validate_and_call(ALIAS, "prx_server".into(), "read".into(), None),
    )
    .await
    .err()
    .context("guard or command-free transport precondition must refuse")?;
    assert!(error.to_string().contains("command is missing"));
    assert!(error.downcast_ref::<ToolExecutionError>().is_none());
    {
        let mut state = tool.state.write();
        state
            .discovered_tools
            .get_mut("prx_server")
            .context("MCP dispatch binding fixture invariant")?
            .get_mut("read")
            .context("MCP dispatch binding fixture invariant")?
            .input_schema = Some(json!({"type":"object","properties":{"changed":{"type":"boolean"}}}));
    }
    tool.rebuild_catalog_snapshot();
    let error = scope_dispatch_binding(
        Some(&binding),
        tool.validate_and_call(ALIAS, "prx_server".into(), "read".into(), None),
    )
    .await
    .err()
    .context("guard or command-free transport precondition must refuse")?;
    assert_eq!(
        error.downcast_ref::<ToolExecutionError>(),
        Some(&ToolExecutionError::StaleCapability)
    );
    tool.state.write().discovered_tools.clear();
    tool.rebuild_catalog_snapshot();
    let error = scope_dispatch_binding(
        Some(&binding),
        tool.validate_and_call(ALIAS, "prx_server".into(), "read".into(), None),
    )
    .await
    .err()
    .context("guard or command-free transport precondition must refuse")?;
    assert_eq!(
        error.downcast_ref::<ToolExecutionError>(),
        Some(&ToolExecutionError::StaleCapability)
    );
    assert!(tool.stdio_sessions.lock().is_empty());
    Ok(())
}
