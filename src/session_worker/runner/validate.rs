use super::capability::{AuthenticatedWorkerTransport, authenticate_worker_capability_with_env};
use super::paths::{
    ensure_child_path, ensure_clean_path, ensure_relative_clean_path, normalized_worker_memory_strategy,
};
use crate::session_worker::protocol::WorkerManifest;
use anyhow::{Context, Result};
use std::path::{Component, Path};

pub(super) fn validate_worker_manifest_with_capability_env(
    manifest: &WorkerManifest,
    env_capability: Option<&str>,
) -> Result<AuthenticatedWorkerTransport> {
    let transport = authenticate_worker_capability_with_env(manifest, env_capability)?;
    anyhow::ensure!(
        manifest.operator_source.is_none()
            || (manifest.ordinary_source.is_none()
                && manifest.child_source.is_none()
                && manifest.capability_manifest.is_none()),
        "session-worker cannot mix operator and delegated source authority"
    );
    if let Some(source) = &manifest.operator_source {
        let key = crate::decision::key::DecisionKey::from_worker_channel(
            &transport.key,
            &source.publication.workspace,
            &manifest.run_id,
            &manifest.config_generation,
        )?;
        let catalog = crate::tools::capability::catalog::CapabilityCatalog::restore_process(
            &source.catalog,
            &key,
            crate::causal_tree::branch::CatalogGeneration(source.publication.catalog_generation),
        )?;
        let publication = source.publication.verify(&key, &catalog)?;
        anyhow::ensure!(
            manifest.owner_id.as_deref() == Some(publication.owner()),
            "operator worker owner differs from publication"
        );
        crate::providers::catalog::CatalogView::restore_operator_process(
            &source.provider_catalog,
            &key,
            publication.id(),
        )?;
        crate::runtime::child_source::parent::ParentSource::restore_operator_process(manifest, &publication, &key)?;
    }
    anyhow::ensure!(
        manifest.ordinary_source.is_none()
            || (manifest.capability_manifest.is_none() && manifest.child_source.is_none()),
        "session-worker cannot mix ordinary and durable source authority"
    );

    // The outer HMAC authenticates transport only. Inner capability seal,
    // durable parent/source lookup and executor parity are admission's job.
    match (&manifest.capability_manifest, &manifest.child_source) {
        (None, None) => {}
        (Some(capabilities), Some(source)) => {
            let parent = crate::causal_tree::ids::DecisionId::parse(&capabilities.parent_decision_id)
                .context("session-worker parent decision id is malformed")?;
            anyhow::ensure!(
                parent.to_string() == capabilities.parent_decision_id
                    && manifest.owner_id.as_deref() == Some(capabilities.owner.as_str())
                    && !capabilities.owner.is_empty()
                    && capabilities.workspace == source.parent_constraints.workspace
                    && !source.parent_store_workspace.is_empty()
                    && source.parent_decision_digest.len() == 64
                    && source
                        .parent_decision_digest
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit())
                    && !source.input_event_id.is_empty()
                    && source.input_event_id.len() <= 256
                    && !source.input_event_id.chars().any(char::is_control)
                    && manifest
                        .parent_run_id
                        .as_deref()
                        .is_some_and(|id| !id.is_empty() && id != manifest.run_id)
                    && !manifest.session_scope_key.is_empty()
                    && manifest.runtime_config_generation_id.is_some()
                    && manifest
                        .runtime_config_source_revision
                        .as_deref()
                        .is_some_and(|revision| !revision.is_empty()),
                "session-worker child source transport is inconsistent"
            );
        }
        _ => anyhow::bail!("session-worker child authority and source must be supplied together"),
    }

    let run_id = manifest.run_id.trim();
    if run_id.is_empty()
        || run_id.contains('/')
        || run_id.contains('\\')
        || run_id.contains("..")
        || run_id.chars().any(char::is_control)
    {
        anyhow::bail!("session-worker run_id must be a single non-empty path-safe segment");
    }

    ensure_clean_path(&manifest.workspace_dir, "workspace_dir")?;
    ensure_clean_path(&manifest.memory_db_path, "memory_db_path")?;
    if !manifest.config_dir.is_absolute()
        || manifest
            .config_dir
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        anyhow::bail!("session-worker config_dir must be an absolute path without parent traversal");
    }
    if manifest.config_generation.len() != 64
        || !manifest.config_generation.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!("session-worker config_generation must be a SHA-256 hex digest");
    }
    if let Some(identity_dir) = manifest
        .identity_dir
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        ensure_relative_clean_path(identity_dir.trim(), "identity_dir")?;
    }

    let strategy = normalized_worker_memory_strategy(manifest)?;
    if let Some(worker_memory_db_path) = manifest.worker_memory_db_path.as_ref() {
        ensure_child_path(worker_memory_db_path, &manifest.workspace_dir, "worker_memory_db_path")?;
    }
    if matches!(strategy, "isolated_private" | "hybrid") {
        let worker_memory_db_path = manifest
            .worker_memory_db_path
            .as_ref()
            .context("worker_memory_db_path is required for isolated/hybrid session-worker memory")?;
        if manifest.memory_db_path != *worker_memory_db_path {
            anyhow::bail!("memory_db_path must match worker_memory_db_path for isolated/hybrid memory");
        }
    }

    if strategy == "shared_fabric" {
        let shared_memory_db_path = manifest
            .shared_memory_db_path
            .as_ref()
            .context("shared_memory_db_path is required for shared_fabric session-worker memory")?;
        ensure_clean_path(shared_memory_db_path, "shared_memory_db_path")?;
        if manifest.memory_db_path != *shared_memory_db_path {
            anyhow::bail!("memory_db_path must match shared_memory_db_path for shared_fabric memory");
        }
    }

    if strategy == "hybrid" {
        let shared_memory_db_path = manifest
            .shared_memory_db_path
            .as_ref()
            .context("shared_memory_db_path is required for hybrid session-worker memory")?;
        ensure_clean_path(shared_memory_db_path, "shared_memory_db_path")?;
    }

    Ok(transport)
}

pub(super) fn validate_worker_cli_overrides(
    manifest: &WorkerManifest,
    task: Option<&str>,
    workspace: Option<&str>,
    memory_db: Option<&str>,
    tools: Option<&[String]>,
    config_dir: Option<&str>,
) -> Result<()> {
    if let Some(task) = task {
        if task != manifest.task {
            anyhow::bail!("session-worker CLI task override must match sealed manifest");
        }
    }
    if let Some(workspace) = workspace {
        if Path::new(workspace) != manifest.workspace_dir {
            anyhow::bail!("session-worker CLI workspace override must match sealed manifest");
        }
    }
    if let Some(memory_db) = memory_db {
        if Path::new(memory_db) != manifest.memory_db_path {
            anyhow::bail!("session-worker CLI memory-db override must match sealed manifest");
        }
    }
    if let Some(tools) = tools {
        if tools != manifest.allowed_tools.as_slice() {
            anyhow::bail!("session-worker CLI tools override must match sealed manifest");
        }
    }
    let config_dir = config_dir
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("session-worker requires the parent --config-dir")?;
    if Path::new(config_dir) != manifest.config_dir {
        anyhow::bail!("session-worker CLI config-dir must match sealed manifest");
    }
    Ok(())
}

pub(super) fn validate_parent_memory_backend(manifest: &WorkerManifest, configured_memory_backend: &str) -> Result<()> {
    if manifest.memory_backend.trim() != configured_memory_backend {
        anyhow::bail!(
            "session-worker parent memory backend mismatch: manifest={}, config={configured_memory_backend}",
            manifest.memory_backend
        );
    }
    Ok(())
}
