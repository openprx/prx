use crate::session_worker::protocol::WorkerManifest;
use anyhow::{Context, Result};
use std::path::{Component, Path};

pub(super) fn parse_tools_override(tools_json: &str) -> Result<Vec<String>> {
    serde_json::from_str(tools_json).with_context(|| "parse --tools JSON as string array")
}

fn path_has_parent_or_prefix(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
}

pub(super) fn ensure_clean_path(path: &Path, label: &str) -> Result<()> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("{label} must not be empty");
    }
    if path_has_parent_or_prefix(path) {
        anyhow::bail!("{label} must not contain parent directory or platform prefix components");
    }
    Ok(())
}

pub(super) fn ensure_relative_clean_path(value: &str, label: &str) -> Result<()> {
    let path = Path::new(value);
    if path.is_absolute() {
        anyhow::bail!("{label} must be relative");
    }
    ensure_clean_path(path, label)
}

pub(super) fn ensure_child_path(path: &Path, root: &Path, label: &str) -> Result<()> {
    ensure_clean_path(path, label)?;
    ensure_clean_path(root, "workspace_dir")?;
    if !path.starts_with(root) {
        anyhow::bail!("{label} must stay under workspace_dir");
    }
    Ok(())
}

pub(super) fn normalized_worker_memory_strategy(manifest: &WorkerManifest) -> Result<&'static str> {
    match manifest.memory_strategy.as_deref().unwrap_or("shared_fabric").trim() {
        "" | "shared_fabric" => Ok("shared_fabric"),
        "isolated_private" => Ok("isolated_private"),
        "hybrid" => anyhow::bail!(crate::config::HYBRID_PROCESS_MEMORY_UNAVAILABLE),
        other => anyhow::bail!("Invalid session-worker memory_strategy '{other}'"),
    }
}
