use super::capability::AuthenticatedWorkerTransport;
#[cfg(test)]
use super::capability::{
    authenticate_worker_capability_with_key, compute_worker_capability_with_key, manifest_signing_payload,
};
use super::context::{
    boxed_worker_segments, load_worker_shared_context, record_hybrid_worker_draft_if_needed,
    with_manifest_spawn_context, worker_lineage_payload, worker_message_event_scope,
};
use super::paths::normalized_worker_memory_strategy;
use super::prompt::{build_worker_system_prompt, build_worker_system_prompt_with_skills};
use super::provider::{validate_worker_route, worker_credentials, worker_provider, worker_spawn_tool};
use super::steering::run_segments_with_steering;
use super::tools::{select_tools_for_manifest, verify_worker_orchestration_tool_names};
use super::validate::validate_parent_memory_backend;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::turn_contracts::ScopeContext;
use crate::agent::turn_engine::run_turn;
use crate::config::Config;
use crate::hooks::HookManager;
use crate::memory::{Memory, MemoryFabric, MessageEventScope};
use crate::observability::NoopObserver;
use crate::providers::{ChatMessage, Provider};
use crate::runtime;
use crate::security::policy::ResourceRiskLevel;
use crate::security::{SecurityPolicy, SideEffectGate};
use crate::session_worker::protocol::{WorkerManifest, WorkerResult, config_source_generation};
use crate::tools;
use crate::tools::sessions_spawn::steering_instruction;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
pub(super) async fn run_validated_manifest(
    mut manifest: WorkerManifest,
    steer_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    explicit_config_dir: Option<&str>,
) -> Result<WorkerResult> {
    anyhow::ensure!(
        manifest.ordinary_source.is_none() && manifest.operator_source.is_none(),
        "worker source requires authenticated transport"
    );
    // Existing direct fixtures use the real full-manifest verifier with an
    // isolated fixture key, rather than bypassing transport with None.
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"prx_worker_transport_fixture_key");
    let payload = manifest_signing_payload(&manifest)?;
    let capability = compute_worker_capability_with_key(&key, &manifest.run_id, 300, &payload);
    manifest.parent_capability = Some(capability.clone());
    let transport = authenticate_worker_capability_with_key(&manifest, Some(&capability), 300, 0, key)?;
    run_authenticated_manifest(manifest, steer_rx, explicit_config_dir, Some(transport)).await
}

pub(super) async fn run_authenticated_manifest(
    manifest: WorkerManifest,
    steer_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    explicit_config_dir: Option<&str>,
    transport: Option<AuthenticatedWorkerTransport>,
) -> Result<WorkerResult> {
    let parent = crate::runtime::turn_cleanup::current_tracker();
    let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(parent.as_ref(), CancellationToken::new())?;
    let prepared = cleanup
        .run(owned_worker_future(manifest, steer_rx, explicit_config_dir, transport))
        .await?;
    if let Some(completion) = prepared.completion {
        crate::session_worker::completion::persist(
            &completion.fabric,
            completion.scope,
            &completion.grant,
            &prepared.result,
        )
        .await?;
    }
    if let Some(authority) = prepared.ordinary_completion {
        crate::session_worker::completion::persist_ordinary(&authority, &prepared.result).await?;
    }
    if let Some(authority) = prepared.operator_completion {
        crate::session_worker::completion::persist_operator(&authority, &prepared.result).await?;
    }
    Ok(prepared.result)
}

struct PendingWorkerCompletion {
    fabric: MemoryFabric,
    scope: MessageEventScope,
    grant: Arc<crate::runtime::child_source::IssuedChildGrant>,
}

struct PreparedWorkerResult {
    _operator_lifetime: Option<crate::runtime::child_source::operator_process::OperatorProcessLifetime>,
    operator_completion: Option<Arc<crate::runtime::child_source::operator_process::OperatorProcessAuthority>>,
    _ordinary_lifetime: Option<crate::runtime::child_source::ordinary_process::OrdinaryProcessLifetime>,
    result: WorkerResult,
    completion: Option<PendingWorkerCompletion>,
    ordinary_completion: Option<Arc<crate::runtime::child_source::ordinary_process::OrdinaryProcessAuthority>>,
}

fn worker_source_workspace(manifest: &WorkerManifest) -> Result<&str> {
    match (
        &manifest.child_source,
        &manifest.ordinary_source,
        &manifest.operator_source,
    ) {
        (Some(source), None, None) => Ok(source.parent_constraints.workspace.as_str()),
        (None, Some(source), None) => Ok(source.parent_constraints.workspace.as_str()),
        (None, None, Some(source)) => Ok(source.publication.workspace.as_str()),
        _ => anyhow::bail!("worker source storage requires one authenticated source"),
    }
}

/// Decision source and settlement storage belong to the logical parent
/// workspace, independently of the worker's private execution memory.
fn worker_decision_fabric(
    manifest: &WorkerManifest,
    generation: &crate::config::ConfigGeneration,
    execution_fabric: &MemoryFabric,
) -> Result<MemoryFabric> {
    let parent_workspace = worker_source_workspace(manifest)?;
    let config = &generation.effective;
    anyhow::ensure!(
        config.workspace_dir.to_string_lossy() == parent_workspace
            && manifest.runtime_config_generation_id == Some(generation.id.0)
            && manifest.runtime_config_source_revision.as_deref()
                == Some(generation.source_revision.fingerprint_sha256.as_str()),
        "worker decision storage differs from verified configuration"
    );
    if normalized_worker_memory_strategy(manifest)? == "shared_fabric" {
        anyhow::ensure!(
            execution_fabric.workspace_id() == parent_workspace,
            "shared worker memory differs from parent workspace"
        );
        return Ok(execution_fabric.clone());
    }
    // Keep execution tools on their existing private SQLite memory. Source
    // verification uses the parent's configured backend and ACL bindings.
    let memory: Arc<dyn Memory> = Arc::from(crate::memory::create_memory_with_storage_and_routes_with_acl(
        &config.memory,
        &config.embedding_routes_with_references(),
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        crate::memory::EmbeddingRootCredential::from_config(config),
        &config.identity_bindings,
        &config.user_policies,
    )?);
    Ok(MemoryFabric::new(memory, parent_workspace).with_event_recording(manifest.memory_event_recording))
}

enum WorkerSegmentSource {
    Operator {
        sequence: tokio::sync::Mutex<crate::runtime::child_source::operator_process::OperatorProcessSegments>,
        authority: Arc<crate::runtime::child_source::operator_process::OperatorProcessAuthority>,
    },
    Granted {
        sequence: tokio::sync::Mutex<crate::runtime::child_source::ChildSegmentSequence>,
        grant: Arc<crate::runtime::child_source::IssuedChildGrant>,
        binding: Arc<crate::runtime::child_source::ChildProviderBinding>,
        provider: Arc<dyn Provider>,
    },
    Ordinary {
        sequence: tokio::sync::Mutex<crate::runtime::child_source::ordinary_process::OrdinaryProcessSegments>,
        authority: Arc<crate::runtime::child_source::ordinary_process::OrdinaryProcessAuthority>,
    },
}

struct WorkerChildSegments {
    source: WorkerSegmentSource,
    service: Arc<crate::runtime::turn_decision::TurnDecisionService>,
    fabric: MemoryFabric,
    owner: Arc<crate::runtime::turn_cleanup::CleanupTracker>,
    usage: Arc<crate::runtime::turn_terminal::TerminalUsageCollector>,
}

impl WorkerChildSegments {
    async fn recorded_count(&self) -> u64 {
        match &self.source {
            WorkerSegmentSource::Granted { sequence, .. } => sequence.lock().await.recorded_count(),
            WorkerSegmentSource::Ordinary { sequence, .. } => sequence.lock().await.recorded_count(),
            WorkerSegmentSource::Operator { sequence, .. } => sequence.lock().await.recorded_count(),
        }
    }

    async fn admit(
        &self,
        input: &str,
        cancel: &CancellationToken,
        acknowledgement: tokio::sync::oneshot::Receiver<Result<(), String>>,
    ) -> Result<crate::runtime::child_turn::ChildSegmentTurn> {
        let (source, config) = match &self.source {
            WorkerSegmentSource::Granted {
                sequence,
                grant,
                binding,
                provider,
            } => {
                let receipt = sequence.lock().await.record(&self.fabric, input).await?;
                let proof = Arc::new(binding.source_proof(grant, &receipt, provider)?);
                let execution = receipt.execution_context(proof.as_ref())?;
                ((proof, execution), Arc::clone(&grant.generation().effective))
            }
            WorkerSegmentSource::Ordinary { sequence, authority } => {
                authority.verify()?;
                let source = sequence.lock().await.record(input).await?;
                (source, Arc::clone(&authority.snapshots().generation.effective))
            }
            WorkerSegmentSource::Operator { sequence, authority } => {
                authority.verify()?;
                let source = sequence.lock().await.record(input).await?;
                (source, Arc::clone(&authority.snapshots().generation.effective))
            }
        };
        Ok(boxed_worker_segments(|| {
            crate::runtime::child_turn::ChildSegmentTurn::admit(
                Arc::clone(&self.service),
                source,
                self.fabric.clone(),
                config,
                &self.owner,
                cancel,
                Some(Arc::clone(&self.usage)),
            )
        })
        .await?
        .with_child_cleanup(acknowledgement))
    }
}

struct VerifiedWorkerParent {
    store: Arc<crate::causal_tree::store::CausalDecisionStore>,
    scope: Arc<crate::tools::capability::child::VerifiedChildScope>,
    catalog: Arc<crate::tools::capability::catalog::CapabilityCatalog>,
}

/// Reconstruct the signed scope and inherited catalog. Actual executor parity,
/// source proof and child terminal ownership remain required before execution.
fn verify_worker_parent_authority(
    manifest: &WorkerManifest,
    logical_workspace: &Path,
) -> Result<Option<VerifiedWorkerParent>> {
    use crate::causal_tree::store::{CausalDecisionStore, DecisionScope};
    use crate::tools::capability::child::{CapabilityConstraints, ChildCapabilityManifest, VerifiedChildScope};
    anyhow::ensure!(
        manifest.ordinary_source.is_none() && manifest.operator_source.is_none(),
        "durable parent verifier rejects non-delegated worker source"
    );
    let (wire, source) = match (&manifest.capability_manifest, &manifest.child_source) {
        (None, None) => return Ok(None),
        (Some(wire), Some(source)) => (wire, source),
        _ => anyhow::bail!("session-worker child authority and source must be supplied together"),
    };
    anyhow::ensure!(
        wire.workspace == source.parent_constraints.workspace,
        "session-worker parent workspace differs from configuration"
    );
    anyhow::ensure!(
        wire.workspace == logical_workspace.to_string_lossy(),
        "session-worker parent workspace differs from configuration"
    );
    let key = crate::decision::key::DecisionKey::load_existing(logical_workspace)?;
    let parent = CapabilityConstraints::from_worker_constraints(&source.parent_constraints, key)?;
    let child = ChildCapabilityManifest::from_worker_manifest(wire, &parent)?;
    let catalog = crate::tools::capability::catalog::CapabilityCatalog::restore_process(
        &source.catalog,
        &parent.key,
        parent.catalog_generation,
    )?;
    for reference in &parent.allowed {
        catalog.resolve(std::slice::from_ref(reference))?;
    }
    let store = CausalDecisionStore::open_unshared_read_only(logical_workspace);
    let store_workspace = store.workspace_id().context("worker parent store namespace missing")?;
    anyhow::ensure!(
        store_workspace.as_ref() == source.parent_store_workspace,
        "worker original store namespace differs from configured decision store"
    );
    let scope = DecisionScope::Owner {
        workspace_id: store_workspace,
        owner_id: Arc::from(parent.authority.owner.as_str()),
    };
    let record = store
        .reader()
        .replay_record(&scope, &parent.decision_id)?
        .context("session-worker parent decision is unavailable")?;
    anyhow::ensure!(
        record.digest == source.parent_decision_digest
            && record.catalog_generation == parent.catalog_generation.to_hex()
            && record.decision.enabled_snapshot
            && !record.decision.legacy_v1
            && record.decision.admits_issued_child()
            && record.decision.run_id == manifest.parent_run_id
            && manifest.owner_id.as_deref() == Some(parent.authority.owner.as_str()),
        "session-worker parent decision differs from issued authority"
    );
    Ok(Some(VerifiedWorkerParent {
        store,
        scope: Arc::new(VerifiedChildScope::new(child, &parent)?),
        catalog,
    }))
}

fn owned_worker_future(
    manifest: WorkerManifest,
    steer_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    explicit_config_dir: Option<&str>,
    transport: Option<AuthenticatedWorkerTransport>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<PreparedWorkerResult>> + Send + '_>> {
    Box::pin(run_owned_manifest(manifest, steer_rx, explicit_config_dir, transport))
}

async fn run_owned_manifest(
    manifest: WorkerManifest,
    mut steer_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    explicit_config_dir: Option<&str>,
    transport: Option<AuthenticatedWorkerTransport>,
) -> Result<PreparedWorkerResult> {
    transport
        .as_ref()
        .context("worker source requires authenticated transport")?
        .verify_manifest(&manifest)?;
    anyhow::ensure!(
        manifest.child_source.is_some() || manifest.ordinary_source.is_some() || manifest.operator_source.is_some(),
        "worker requires an authenticated decision source"
    );
    let explicit_config_dir = explicit_config_dir
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("session-worker requires the parent --config-dir")?;
    if Path::new(explicit_config_dir) != manifest.config_dir {
        anyhow::bail!("session-worker config source does not match sealed manifest");
    }

    let generation_before = match config_source_generation(&manifest.config_dir) {
        Ok(generation) => generation,
        Err(error) if manifest.operator_source.is_some() => {
            let authenticated = transport.as_ref().context("operator worker transport missing")?;
            // This witness exists only before config load and any provider/segment work.
            let failure = crate::session_worker::startup_failure::AuthenticatedStartupFailure::attest(
                &authenticated.key,
                authenticated.manifest_digest,
                &manifest.run_id,
                error.to_string(),
            )?;
            return Err(failure.into());
        }
        Err(error) => return Err(error),
    };
    if generation_before != manifest.config_generation {
        anyhow::bail!(
            "session-worker config generation mismatch before load: expected {}, found {}",
            manifest.config_generation,
            generation_before
        );
    }
    let mut config = Config::load_existing_read_only_with_config_dir(Some(explicit_config_dir)).await?;
    // The worker is a separate process and never reaches `runtime::mode::dispatch`,
    // so it installs the hang-detection thresholds itself. Without this it would
    // silently run on the built-in defaults instead of the operator's config.
    crate::agent::idle::install(config.runtime.idle_hang_secs);
    let generation_after = config_source_generation(&manifest.config_dir)?;
    if generation_after != manifest.config_generation {
        anyhow::bail!(
            "session-worker config generation changed during load: expected {}, found {}",
            manifest.config_generation,
            generation_after
        );
    }
    // Config loading resolves relative paths from the worker's physical
    // config directory. Restore the authenticated parent's spelling before
    // checking signed parent snapshots, while requiring the same filesystem
    // identity as the loaded configuration.
    let source_workspace = worker_source_workspace(&manifest)?;
    anyhow::ensure!(
        crate::config::generation::process_paths_match(&config.workspace_dir, Path::new(source_workspace))?,
        "session-worker parent workspace differs from configuration"
    );
    config.workspace_dir = PathBuf::from(source_workspace);
    let operator_snapshots = if manifest.operator_source.is_some() {
        Some(super::operator_source::restore(
            &manifest,
            config.clone(),
            transport.as_ref().context("operator worker transport missing")?,
        )?)
    } else {
        None
    };
    let ordinary_snapshots = if manifest.ordinary_source.is_some() {
        Some(super::ordinary_source::restore(
            &manifest,
            config.clone(),
            transport.as_ref().context("ordinary worker transport missing")?,
        )?)
    } else {
        None
    };
    let child_parent = if ordinary_snapshots.is_some() || operator_snapshots.is_some() {
        None
    } else {
        verify_worker_parent_authority(&manifest, &config.workspace_dir)?
    };
    let child_generation = match (child_parent.as_ref(), manifest.child_source.as_ref()) {
        (Some(parent_authority), Some(source)) => {
            anyhow::ensure!(
                manifest.runtime_config_generation_id == Some(source.config_generation.id.0)
                    && manifest.runtime_config_source_revision.as_deref()
                        == Some(source.config_generation.source_revision.fingerprint_sha256.as_str()),
                "session-worker configuration binding differs from source lineage"
            );
            let (_, parent) = parent_authority.scope.gate_inputs();
            Some(crate::config::ConfigGeneration::restore_process(
                config.clone(),
                &source.config_generation,
                &parent.key,
            )?)
        }
        (None, None) => None,
        _ => anyhow::bail!("session-worker verified scope and source binding differ"),
    };
    anyhow::ensure!(
        !config.causal_tree.enabled || child_parent.is_some() || operator_snapshots.is_some(),
        "decision-enabled session-worker requires authenticated child authority"
    );

    if child_parent.is_none() && ordinary_snapshots.is_none() && operator_snapshots.is_none() {
        validate_worker_route(&manifest, &config)?;
    }
    let configured_memory_backend =
        crate::memory::effective_memory_backend_name(&config.memory.backend, Some(&config.storage.provider.config));
    validate_parent_memory_backend(&manifest, &configured_memory_backend)?;
    config.workspace_dir = manifest.workspace_dir.clone();
    // FIX-P1-31: honour the configured `security.audit` block on the gate audit path.
    let security = Arc::new(
        SecurityPolicy::from_config(&config.autonomy, &manifest.workspace_dir)
            .with_audit_config(config.security.audit.clone()),
    );

    let provider_runtime_options = crate::providers::provider_runtime_options_from_config(&config);

    #[cfg(feature = "wasm-plugins")]
    let wasm_early_runtime =
        if manifest.provider_name.starts_with("wasm:") || configured_memory_backend.starts_with("wasm:") {
            crate::plugins::init_plugin_runtime(&manifest.workspace_dir, None).await
        } else {
            None
        };

    let credentials = worker_credentials(&manifest, &config);
    let legacy_provider: Option<Arc<dyn Provider>> =
        if child_parent.is_none() && ordinary_snapshots.is_none() && operator_snapshots.is_none() {
            Some(Arc::from(worker_provider(
                &manifest,
                &config,
                &credentials,
                &provider_runtime_options,
            )?))
        } else {
            None
        };

    let memory: Arc<dyn Memory> = if normalized_worker_memory_strategy(&manifest)? == "shared_fabric" {
        let parent_workspace = manifest
            .memory_workspace_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .context("shared_fabric session-worker requires parent memory_workspace_id")?;
        Arc::from(crate::memory::create_memory_with_storage_and_routes_with_acl(
            &config.memory,
            &config.embedding_routes_with_references(),
            Some(&config.storage.provider.config),
            &parent_workspace,
            crate::memory::EmbeddingRootCredential::from_config(&config),
            &config.identity_bindings,
            &config.user_policies,
        )?)
    } else {
        Arc::new(crate::memory::SqliteMemory::new_with_path_and_acl(
            manifest.memory_db_path.clone(),
            config.memory.acl_enabled,
        )?)
    };
    #[cfg(feature = "wasm-plugins")]
    let wasm_plugin_runtime = if let Some(runtime) = wasm_early_runtime {
        runtime
            .attach_memory(Arc::clone(&memory))
            .await
            .map_err(|error| anyhow::anyhow!("failed to attach WASM memory backend in session-worker: {error}"))?;
        Some(runtime)
    } else {
        crate::plugins::init_plugin_runtime(&manifest.workspace_dir, Some(Arc::clone(&memory))).await
    };
    let memory_workspace_id = manifest
        .memory_workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| manifest.workspace_dir.to_string_lossy().to_string());
    let memory_fabric =
        MemoryFabric::new(memory.clone(), memory_workspace_id).with_event_recording(manifest.memory_event_recording);
    let decision_fabric = child_generation
        .as_ref()
        .or_else(|| ordinary_snapshots.as_ref().map(|source| &source.generation))
        .or_else(|| operator_snapshots.as_ref().map(|source| &source.generation))
        .map(|generation| worker_decision_fabric(&manifest, generation, &memory_fabric))
        .transpose()?;
    let (ordinary_authority, ordinary_lifetime) = if let Some(source) = ordinary_snapshots {
        let (authority, lifetime) = source
            .issue(
                &manifest,
                decision_fabric
                    .as_ref()
                    .context("ordinary worker source storage missing")?
                    .clone(),
                &CancellationToken::new(),
            )
            .await?;
        (Some(authority), Some(lifetime))
    } else {
        (None, None)
    };
    let (operator_authority, operator_lifetime) = if let Some(source) = operator_snapshots {
        let (authority, lifetime) = source
            .issue(
                &manifest,
                decision_fabric
                    .as_ref()
                    .context("operator worker source storage missing")?
                    .clone(),
                &CancellationToken::new(),
            )
            .await?;
        (Some(authority), Some(lifetime))
    } else {
        (None, None)
    };
    let worker_event_scope = ordinary_authority.as_ref().map_or_else(
        || worker_message_event_scope(&manifest),
        |authority| authority.logical_scope(),
    );
    let worker_event_scope = operator_authority
        .as_ref()
        .map_or(worker_event_scope, |authority| authority.logical_scope());
    let source_receipt = if let Some(parent) = child_parent.as_ref() {
        let source_fabric = decision_fabric.as_ref().context("worker decision storage is missing")?;
        Some(crate::runtime::child_source::restore_worker_input(source_fabric, &manifest, &parent.scope).await?)
    } else if ordinary_authority.is_some() || operator_authority.is_some() {
        // The parent's optional original input was already read back by issuance.
        None
    } else {
        SideEffectGate::new(security.as_ref())
            .authorize_resource_operation(
                "session_worker",
                &format!("session_worker:request_event:{}", manifest.run_id),
                ResourceRiskLevel::Low,
                None,
            )
            .map_err(anyhow::Error::msg)?;
        crate::runtime::child_source::record_child_input(
            &memory_fabric,
            worker_event_scope.clone(),
            &manifest.task,
            Some(format!("session_worker:{}:request", manifest.run_id)),
            Some(worker_lineage_payload(&manifest).to_string()),
            None,
        )
        .await?
    };

    let parent_source = if let Some(parent) = child_parent.as_ref() {
        Some(
            crate::runtime::child_source::parent::ParentSource::restore_process(
                &manifest,
                &parent.store,
                &parent.scope,
                decision_fabric.as_ref().context("worker decision storage is missing")?,
                child_generation
                    .as_deref()
                    .context("worker child generation is missing")?,
                source_receipt.as_deref().context("worker child input is missing")?,
            )
            .await?,
        )
    } else {
        None
    };

    let child_grant = if let Some(provenance) = parent_source {
        let authority = child_parent.as_ref().context("worker parent authority missing")?;
        let source = manifest
            .child_source
            .as_ref()
            .context("worker source binding missing")?;
        let (_, constraints) = authority.scope.gate_inputs();
        let provider_catalog = crate::providers::catalog::CatalogView::restore_process(
            &source.provider_catalog,
            &constraints.key,
            constraints.decision_id,
        )?;
        Some(crate::runtime::child_source::IssuedChildGrant::restore_process(
            Arc::clone(&authority.scope),
            provenance,
            Arc::clone(&authority.catalog),
            Arc::clone(child_generation.as_ref().context("worker child generation missing")?),
            provider_catalog,
            (
                Arc::from(source.parent_store_workspace.as_str()),
                source.parent_decision_digest.clone(),
            ),
            crate::runtime::child_source::ChildGrantRuntime {
                decision_store: Arc::clone(&authority.store),
                source_fabric: decision_fabric
                    .as_ref()
                    .context("worker logical source fabric missing")?
                    .clone(),
                security: Arc::clone(&security),
            },
            source_receipt.as_deref().context("worker child input missing")?,
            &manifest,
            transport
                .as_ref()
                .context("worker execution workspace is not authenticated")?,
        )?)
    } else {
        None
    };

    // Logical completion belongs to the authenticated original spawn input.
    // Worker model segments and worker-result events keep their own scope.
    let logical_task_scope = if child_grant.is_some() {
        source_receipt
            .as_deref()
            .context("worker logical input missing")?
            .logical_task_scope()
    } else {
        worker_event_scope.clone()
    };

    // Capture completion-only authority before the driver can be cancelled.
    // Model segment authority remains independently checked per segment.
    let logical_terminal = match (
        child_grant.as_deref(),
        ordinary_authority.as_deref(),
        operator_authority.as_deref(),
    ) {
        (Some(grant), None, None) => crate::agent::terminal::logical::LogicalTaskTerminalRef::granted(
            grant,
            source_receipt.as_deref().context("worker logical input missing")?,
            &logical_task_scope,
        )?,
        (None, Some(authority), None) => {
            crate::agent::terminal::logical::LogicalTaskTerminalRef::ordinary_process(authority)?
        }
        (None, None, Some(authority)) => {
            crate::agent::terminal::logical::LogicalTaskTerminalRef::operator_process(authority)?
        }
        _ => anyhow::bail!("worker logical completion requires one authenticated source authority"),
    };

    let child_provider_binding = child_grant
        .as_ref()
        .map(|grant| {
            crate::runtime::child_source::ChildProviderBinding::build_worker(Arc::clone(grant), &manifest).map(Arc::new)
        })
        .transpose()?;
    let provider: Arc<dyn Provider> = if let Some(binding) = child_provider_binding.as_ref() {
        binding.provider()
    } else if let Some(authority) = &operator_authority {
        authority.binding().provider()
    } else if let Some(authority) = &ordinary_authority {
        authority.binding().provider()
    } else {
        legacy_provider.context("legacy worker provider missing")?
    };
    let child_source_proof = if let Some(binding) = child_provider_binding.as_ref() {
        Some(binding.source_proof(
            child_grant.as_deref().context("worker issued grant missing")?,
            source_receipt.as_deref().context("worker child input missing")?,
            &provider,
        )?)
    } else if let Some(authority) = &operator_authority {
        Some(authority.source_proof()?)
    } else if let Some(authority) = &ordinary_authority {
        Some(authority.source_proof()?)
    } else {
        None
    };

    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    let shared_config = crate::config::new_shared(config.clone());
    let hooks = Arc::new(HookManager::new(manifest.workspace_dir.clone()).with_config(Arc::clone(&shared_config)));
    #[cfg(feature = "wasm-plugins")]
    if let Some(plugin_runtime) = &wasm_plugin_runtime {
        hooks.set_plugin_runtime(Arc::clone(plugin_runtime)).await;
    }

    let (composio_key, composio_entity_id) = if config.composio.configured() {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };

    #[allow(unused_mut)]
    let mut extensions = hooks.control_tool_arcs(Arc::clone(&security));
    #[cfg(feature = "wasm-plugins")]
    if let Some(plugin_runtime) = &wasm_plugin_runtime {
        extensions.extend(plugin_runtime.control_tool_arcs(Arc::clone(&security)));
    }
    let mut full_tools = tools::all_tools_with_runtime_ext_and_extensions(
        Arc::new(config.clone()),
        shared_config.clone(),
        &security,
        runtime,
        memory.clone(),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &manifest.workspace_dir,
        &config.agents_with_references(),
        credentials.root_api_key,
        &config,
        extensions,
    )
    .tools;

    // Recreate the same orchestration surfaces that owning entrypoints append
    // after the base factory. Without this block process workers silently lose
    // sessions_spawn and its shared-registry siblings even with wildcard tool
    // inheritance.
    let active_runs: Arc<tokio::sync::RwLock<Vec<crate::tools::sessions_spawn::SubAgentRun>>> =
        Arc::new(tokio::sync::RwLock::new(Vec::new()));
    let session_fabric = decision_fabric.clone().unwrap_or_else(|| {
        MemoryFabric::new(
            Arc::clone(&memory),
            manifest.workspace_dir.to_string_lossy().to_string(),
        )
        .with_event_recording(config.memory.event_recording_config())
    });
    let session_memory = session_fabric.memory();
    let workspace_id = session_fabric.workspace_id().to_owned();
    let spawn_tool = worker_spawn_tool(
        &manifest,
        &config,
        &credentials,
        #[cfg(test)]
        Arc::clone(&provider),
        Arc::clone(&security),
        &provider_runtime_options,
        Arc::clone(&active_runs),
    )
    .with_shared_memory(Arc::clone(&memory))
    .with_source_fabric(session_fabric.clone())
    .with_event_recording(config.memory.event_recording_config());
    let spawn_tools_handle = spawn_tool.tools_handle();
    full_tools.push(Box::new(
        crate::tools::SessionsListTool::new(Arc::clone(&active_runs))
            .with_shared_memory(Arc::clone(&session_memory), workspace_id.clone()),
    ));
    full_tools.push(Box::new(
        crate::tools::SessionsSendTool::with_security(Arc::clone(&active_runs), Arc::clone(&security))
            .with_shared_memory(Arc::clone(&session_memory))
            .with_source_fabric(session_fabric.clone())
            .with_event_recording(config.memory.event_recording_config()),
    ));
    full_tools.push(Box::new(
        crate::tools::SubagentsTool::with_security(Arc::clone(&active_runs), Arc::clone(&security))
            .with_shared_memory(Arc::clone(&session_memory))
            .with_source_fabric(session_fabric.clone())
            .with_event_recording(config.memory.event_recording_config()),
    ));
    full_tools.push(Box::new(
        crate::tools::SessionsHistoryTool::new(Arc::clone(&active_runs))
            .with_shared_memory(Arc::clone(&session_memory), workspace_id.clone()),
    ));
    full_tools.push(Box::new(
        crate::tools::SessionStatusTool::new(
            Arc::clone(&active_runs),
            &manifest.provider_name,
            &manifest.model,
            vec!["session-worker".to_string()],
        )
        .with_shared_memory(Arc::clone(&session_memory), workspace_id),
    ));
    full_tools.push(Box::new(spawn_tool));
    full_tools.push(Box::new(crate::tools::DaemonMessageSendTool::from_config(
        &config,
        Arc::clone(&security),
    )));
    full_tools.push(Box::new(
        crate::tools::ImageTool::new(
            Arc::clone(&provider),
            &manifest.model,
            manifest.temperature,
            Arc::clone(&security),
            config.multimodal.clone(),
        )
        .with_provider_name(&manifest.provider_name)
        .with_model_catalog(crate::providers::selection::catalog_for(&config)),
    ));
    full_tools.push(Box::new(crate::tools::ConfigReloadTool::with_security(
        shared_config.clone(),
        Arc::clone(&security),
    )));
    full_tools.push(Box::new(
        crate::tools::GatewayTool::new(
            shared_config,
            &manifest.provider_name,
            &manifest.model,
            vec!["session-worker".to_string()],
        )
        .with_tools_count(full_tools.len() + 1)
        .with_security(Arc::clone(&security)),
    ));

    verify_worker_orchestration_tool_names(full_tools.iter().map(|tool| tool.name()))?;

    let tools_registry = select_tools_for_manifest(full_tools, &manifest).await?;
    let _ = spawn_tools_handle.set(&tools_registry);
    let child_service = if let Some(grant) = child_grant.as_ref() {
        Some(
            crate::runtime::turn_publication::ChildDecisionPublication {
                allowed_tools: None,
                grant: Arc::clone(grant),
                registry: Arc::clone(&tools_registry),
                observer: Arc::new(NoopObserver),
            }
            .publish()
            .await?,
        )
    } else if let Some(authority) = &ordinary_authority {
        let mut excluded_names: std::collections::BTreeSet<String> =
            config.tool_tiering.always_exclude.iter().cloned().collect();
        excluded_names.extend(manifest.channel_excluded_tools.iter().cloned());
        Some(
            crate::runtime::turn_publication::OrdinaryProcessDecisionPublication {
                authority: Arc::clone(authority),
                registry: Arc::clone(&tools_registry),
                security: Arc::clone(&security),
                runtime: crate::runtime::turn_decision::TurnLoopRuntime {
                    hooks: Arc::clone(&hooks),
                    observer: Arc::new(NoopObserver),
                },
                surface_policy: crate::tools::capability::catalog::SurfacePolicy {
                    allowed_tools: if manifest.allowed_tools.is_empty()
                        || manifest.allowed_tools.iter().any(|name| name == "*")
                    {
                        None
                    } else {
                        Some(manifest.allowed_tools.iter().cloned().collect())
                    },
                    excluded_names,
                    direct_names: config.tool_tiering.always_include.iter().cloned().collect(),
                    channel_allowed: false,
                },
            }
            .publish()
            .await?,
        )
    } else if let Some(authority) = &operator_authority {
        let mut excluded_names: std::collections::BTreeSet<String> =
            config.tool_tiering.always_exclude.iter().cloned().collect();
        excluded_names.extend(manifest.channel_excluded_tools.iter().cloned());
        Some(
            crate::runtime::turn_publication::OperatorProcessDecisionPublication {
                authority: Arc::clone(authority),
                registry: Arc::clone(&tools_registry),
                security: Arc::clone(&security),
                runtime: crate::runtime::turn_decision::TurnLoopRuntime {
                    hooks: Arc::clone(&hooks),
                    observer: Arc::new(NoopObserver),
                },
                surface_policy: crate::tools::capability::catalog::SurfacePolicy {
                    allowed_tools: if manifest.allowed_tools.is_empty()
                        || manifest.allowed_tools.iter().any(|name| name == "*")
                    {
                        None
                    } else {
                        Some(manifest.allowed_tools.iter().cloned().collect())
                    },
                    excluded_names,
                    direct_names: config.tool_tiering.always_include.iter().cloned().collect(),
                    channel_allowed: false,
                },
            }
            .publish()
            .await?,
        )
    } else {
        None
    };
    let native_tools = match (child_service.as_ref(), child_source_proof.as_ref()) {
        (Some(service), Some(proof)) => service.model_can_call_tools(proof),
        (None, None) => {
            provider
                .capabilities_for(
                    &manifest.model,
                    crate::providers::traits::ProviderRequestMode::NonStreaming,
                )
                .native_tool_calling
        }
        _ => anyhow::bail!("worker child service and source proof differ"),
    };
    let provider_started_at = chrono::Utc::now();

    let child_segments = if let Some(grant) = child_grant.as_ref() {
        Some(Arc::new(WorkerChildSegments {
            source: WorkerSegmentSource::Granted {
                sequence: tokio::sync::Mutex::new(crate::runtime::child_source::ChildSegmentSequence::new_worker(
                    Arc::clone(grant),
                    Arc::clone(source_receipt.as_ref().context("worker child input missing")?),
                )?),
                grant: Arc::clone(grant),
                binding: Arc::clone(
                    child_provider_binding
                        .as_ref()
                        .context("worker provider binding missing")?,
                ),
                provider: Arc::clone(&provider),
            },
            service: Arc::clone(child_service.as_ref().context("worker child service missing")?),
            fabric: decision_fabric
                .as_ref()
                .context("worker decision storage missing")?
                .clone(),
            owner: crate::runtime::turn_cleanup::current_tracker().context("worker cleanup owner missing")?,
            usage: Arc::new(crate::runtime::turn_terminal::TerminalUsageCollector::new(
                grant.parent().workspace_id().to_owned(),
                grant.parent().owner_id().to_owned(),
                manifest.run_id.clone(),
            )),
        }))
    } else if let Some(authority) = &ordinary_authority {
        Some(Arc::new(WorkerChildSegments {
            source: WorkerSegmentSource::Ordinary {
                sequence: tokio::sync::Mutex::new(authority.segments()?),
                authority: Arc::clone(authority),
            },
            service: Arc::clone(child_service.as_ref().context("ordinary worker service missing")?),
            fabric: authority.source_fabric().clone(),
            owner: crate::runtime::turn_cleanup::current_tracker().context("ordinary worker cleanup owner missing")?,
            usage: Arc::new(
                crate::runtime::turn_terminal::TerminalUsageCollector::new_ordinary(
                    authority.snapshots().parent.workspace_id().to_owned(),
                    authority.snapshots().parent.owner_id().to_owned(),
                    manifest.run_id.clone(),
                    authority
                        .snapshots()
                        .generation
                        .effective
                        .memory
                        .event_recording_config(),
                )
                .with_ordinary_journal(
                    crate::runtime::child_source::ordinary_process::journal::OrdinarySegmentJournal::new(authority)?,
                ),
            ),
        }))
    } else if let Some(authority) = &operator_authority {
        Some(Arc::new(WorkerChildSegments {
            source: WorkerSegmentSource::Operator {
                sequence: tokio::sync::Mutex::new(authority.segments()?),
                authority: Arc::clone(authority),
            },
            service: Arc::clone(child_service.as_ref().context("operator worker service missing")?),
            fabric: authority.source_fabric().clone(),
            owner: crate::runtime::turn_cleanup::current_tracker().context("operator worker cleanup owner missing")?,
            usage: Arc::new(
                crate::runtime::turn_terminal::TerminalUsageCollector::new_ordinary(
                    authority.snapshots().parent.workspace_id().to_owned(),
                    authority.snapshots().parent.owner_id().to_owned(),
                    manifest.run_id.clone(),
                    authority
                        .snapshots()
                        .generation
                        .effective
                        .memory
                        .event_recording_config(),
                )
                .with_operator_journal(
                    crate::runtime::child_source::operator_process::journal::OperatorSegmentJournal::new(authority)?,
                ),
            ),
        }))
    } else {
        None
    };

    let run_future = boxed_worker_segments(|| async {
        let initial_history = Vec::new();

        let observer = NoopObserver;
        let scope_ctx = match (
            manifest.scope_sender.as_deref(),
            manifest.scope_channel.as_deref(),
            manifest.scope_chat_type.as_deref(),
            manifest.scope_chat_id.as_deref(),
        ) {
            (Some(sender), Some(channel), Some(chat_type), Some(chat_id))
                if !sender.is_empty() && !channel.is_empty() && !chat_type.is_empty() && !chat_id.is_empty() =>
            {
                Some(ScopeContext {
                    policy: &security,
                    sender,
                    channel,
                    chat_type,
                    chat_id,
                    session_key: Some(manifest.session_scope_key.as_str()),
                    owner_id: manifest.owner_id.as_deref(),
                    owner_isolated: manifest.owner_isolated,
                    topic_id: manifest.topic_id.as_deref(),
                    task_id: manifest.parent_task_id.as_deref(),
                    source_message_event_id: manifest.source_message_event_id.as_deref(),
                    config_generation_id: manifest.runtime_config_generation_id,
                    config_source_revision: manifest.runtime_config_source_revision.as_deref(),
                })
            }
            _ => None,
        };
        // Borrow everything the segment needs once, outside the closure: an
        // `async move` segment future must capture `Copy` references, never move
        // fields out of the `FnMut` closure's captured environment.
        let provider_ref = provider.as_ref();
        let observer_ref = &observer;
        let hooks_ref = hooks.as_ref();
        let scope_ctx_ref = scope_ctx.as_ref();
        let memory_ref = &memory;
        let provider_name_ref: &str = &manifest.provider_name;
        let model_ref: &str = &manifest.model;
        let workspace_dir_ref: &Path = &manifest.workspace_dir;
        let compaction_config_ref = manifest.compaction_config.as_ref();
        let multimodal_ref = &config.multimodal;
        let low_priority_tools_ref: &[String] = &config.agent.low_priority_tools;
        let temperature = manifest.temperature;
        let read_only_window = config.agent.read_only_tool_concurrency_window;
        let priority_scheduling_enabled = config.agent.priority_scheduling_enabled;
        let tools_registry_ref = &tools_registry;
        let memory_fabric_ref = &memory_fabric;
        let child_segments_ref = &child_segments;
        let security_ref = security.as_ref();
        let raw_task = manifest.task.as_str();
        let manifest_ref = &manifest;
        let config_ref = &config;
        let (history, loop_result) = run_segments_with_steering(
            initial_history,
            &mut steer_rx,
            |mut segment_history, cancel, raw_steer| {
                let tools_registry = Arc::clone(tools_registry_ref);
                let memory = Arc::clone(memory_ref);
                let child_segments = child_segments_ref.clone();
                let request_event_fabric = child_segments
                    .as_ref()
                    .map_or_else(|| memory_fabric_ref.clone(), |segments| segments.fabric.clone());
                async move {
                    let parent = crate::runtime::turn_cleanup::current_tracker();
                    let mut cleanup = match crate::runtime::turn_cleanup::CleanupScope::new(
                        parent.as_ref(),
                        CancellationToken::new(),
                    ) {
                        Ok(cleanup) => cleanup,
                        Err(error) => {
                            return (
                                segment_history,
                                Err(error.context(crate::runtime::turn_cleanup::CleanupIncomplete)),
                            );
                        }
                    };
                    let child_turn = if let Some(segments) = child_segments.as_ref() {
                        let (sender, receiver) = tokio::sync::oneshot::channel();
                        cleanup = cleanup.with_acknowledgement(Some(sender));
                        let steering_input = raw_steer.as_deref().map(steering_instruction);
                        match boxed_worker_segments(|| {
                            segments.admit(steering_input.as_deref().unwrap_or(raw_task), &cancel, receiver)
                        })
                        .await
                        {
                            Ok(turn) => Some(turn),
                            Err(error) => return (segment_history, Err(error)),
                        }
                    } else {
                        None
                    };
                    if let Some(turn) = child_turn.as_ref() {
                        match turn.is_close() {
                            Ok(true) => {
                                let closed = async {
                                    cleanup.run(async { Ok(()) }).await?;
                                    let reply =
                                        turn.close(&cancel).await?.context("worker close disposition changed")?;
                                    Ok((reply, crate::agent::turn_contracts::ToolLoopTrace::default()))
                                }
                                .await;
                                if let Ok((reply, _)) = &closed {
                                    segment_history.push(ChatMessage::assistant(reply.clone()));
                                }
                                return (segment_history, closed);
                            }
                            Ok(false) => {}
                            Err(error) => return (segment_history, Err(error)),
                        }
                    }
                    let initial_input = segment_history.is_empty();
                    let selected_prompt = if let Some(turn) = child_turn.as_ref() {
                        match turn.prompt_skills() {
                            Ok(skills) => Some(build_worker_system_prompt_with_skills(
                                manifest_ref,
                                config_ref,
                                native_tools,
                                &skills,
                            )),
                            Err(error) => return (segment_history, Err(error)),
                        }
                    } else if segment_history.first().is_none_or(|message| message.role != "system") {
                        Some(build_worker_system_prompt(manifest_ref, config_ref, native_tools))
                    } else {
                        None
                    };
                    if let Some(prompt) = selected_prompt {
                        if let Some(system) = segment_history.first_mut().filter(|message| message.role == "system") {
                            *system = ChatMessage::system(prompt);
                        } else {
                            segment_history.insert(0, ChatMessage::system(prompt));
                        }
                    }
                    if initial_input {
                        let user_task = if child_turn.is_some() {
                            raw_task.to_owned()
                        } else {
                            let shared_context =
                                load_worker_shared_context(manifest_ref, config_ref, memory.as_ref()).await;
                            if shared_context.trim().is_empty() {
                                raw_task.to_owned()
                            } else {
                                format!("{shared_context}{raw_task}")
                            }
                        };
                        segment_history.push(crate::agent::turn_contracts::inbound_user_message(&user_task));
                    }
                    if let Some(turn) = child_turn.as_ref() {
                        if let Err(error) = turn.enrich_input(&mut segment_history) {
                            let result = cleanup.run(async { Err::<(), _>(error) }).await;
                            let error = result
                                .err()
                                .unwrap_or_else(|| anyhow::anyhow!("worker enrichment failed"));
                            if let Err(settlement) = turn.fail(&error, cancel.is_cancelled()).await {
                                return (segment_history, Err(settlement));
                            }
                            return (segment_history, Err(error));
                        }
                    }
                    let Some(admitted_turn) = child_turn.as_ref() else {
                        return (
                            segment_history,
                            Err(anyhow::anyhow!("worker decision admission missing")),
                        );
                    };
                    let adapter = child_turn.as_ref().map(|turn| turn.adapter());
                    let child_execution = adapter.as_ref().map(|adapter| adapter.tool_execution_context.clone());
                    let child_scope = child_execution.as_ref().map(|context| {
                        let envelope = &context.envelope;
                        ScopeContext::from_execution_context(
                            security_ref,
                            context,
                            envelope.recipient.as_deref().unwrap_or_default(),
                        )
                    });
                    let loop_scope = child_scope.as_ref().or(scope_ctx_ref);
                    let loop_result = cleanup
                        .run(run_turn(
                            &mut segment_history,
                            crate::agent::turn_request::TurnRequest {
                                channel_name: "session-worker",
                                scope: loop_scope,
                                mode: crate::agent::turn_contracts::ChatMode::default(),
                                silent: true,
                                expose_stay_silent: false,
                                multimodal: multimodal_ref,
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: read_only_window,
                                    priority_scheduling_enabled,
                                    low_priority_tool_names: low_priority_tools_ref.to_vec(),
                                },
                                compaction: child_turn
                                    .as_ref()
                                    .map(|turn| turn.compaction())
                                    .or(compaction_config_ref),
                                // The parent already selected and sealed this worker's
                                // capability set. Reapplying language-dependent intent
                                // tiering here would silently hide inherited tools and
                                // make process mode weaker than task mode.
                                decision: admitted_turn.routing(),
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: provider_ref,
                                provider_name: provider_name_ref,
                                model: model_ref,
                                temperature,
                                tools_registry,
                                observer: observer_ref,
                                hooks: hooks_ref,
                                approval: None,
                                approval_resolver: None,
                                runtime_adapter: adapter,
                                // The ledger comes from `memory` directly: a session worker without a
                                // resolved ingest scope must still be able to run side-effecting tools.
                                memory: crate::agent::turn_request::ToolLoopMemory::new(
                                    &memory,
                                    workspace_dir_ref,
                                    loop_scope.map(|ctx| {
                                        DocumentIngestRuntime::from_scope(Arc::clone(&memory), ctx).into_nested_turn()
                                    }),
                                )
                                .with_event_fabric(request_event_fabric),
                            },
                            crate::agent::turn_request::TurnDelivery {
                                // Steering cancels the in-flight segment; without a token the
                                // worker could only append after the loop finished on its own.
                                cancellation: Some(cancel.clone()),
                                on_delta: None,
                                on_tool_call: None,
                            },
                        ))
                        .await;
                    if let Some(turn) = child_turn.as_ref() {
                        let settlement = match &loop_result {
                            Ok((outcome, trace)) => turn.complete(outcome, trace).await.map(|_| ()),
                            Err(error) => turn.fail(error, cancel.is_cancelled()).await,
                        };
                        if let Err(error) = settlement {
                            return (segment_history, Err(error));
                        }
                    }
                    let loop_result = loop_result.map(|(outcome, trace)| (outcome.into_text(), trace));
                    (segment_history, loop_result)
                }
            },
        )
        .await;
        (loop_result, history.len())
    });

    let run_future = with_manifest_spawn_context(
        &manifest,
        source_receipt,
        child_grant.clone(),
        child_provider_binding.clone(),
        child_segments.as_ref().map(|segments| Arc::clone(&segments.usage)),
        run_future,
    );

    let result = run_future.await;

    let (loop_result, history_commit_len) = result;
    let (mut worker_result, terminal_status) = match loop_result {
        Ok((output, trace)) => {
            let tokens_used = trace.tokens_used;
            (
                WorkerResult {
                    completion_evidence: None,
                    ordinary_completion: None,
                    operator_completion: None,
                    success: true,
                    output: if output.trim().is_empty() {
                        "[Sub-agent produced no output]".to_string()
                    } else {
                        output
                    },
                    error: None,
                    tokens_used: tokens_used.has_any_tokens().then_some(tokens_used),
                },
                crate::agent::terminal::TurnTerminalStatus::Completed,
            )
        }
        Err(error) => (
            WorkerResult {
                completion_evidence: None,
                ordinary_completion: None,
                operator_completion: None,
                success: false,
                output: String::new(),
                error: Some(error.to_string()),
                tokens_used: None,
            },
            crate::agent::terminal::TurnTerminalStatus::Failed,
        ),
    };

    if let Some(segments) = child_segments.as_ref() {
        let recorded = segments.recorded_count().await;
        let confirmed = u64::try_from(segments.usage.confirmed_count())?;
        anyhow::ensure!(
            !worker_result.success || (recorded > 0 && recorded == confirmed),
            "worker cannot commit completion with unconfirmed segments"
        );
    }

    // Shared child segments already own provider settlement. The logical
    // worker terminal records only completion and delivery, in the same
    // logical source store as its admitted input and segment terminals.
    let terminal_fabric = decision_fabric.as_ref().unwrap_or(&memory_fabric);
    let terminal_receipt = logical_terminal
        .finalize(
            terminal_fabric,
            crate::agent::terminal::TurnTerminalDraft {
                decision: None,
                terminal_id: manifest.run_id.clone(),
                scope: logical_task_scope.clone(),
                status: terminal_status,
                history: worker_result
                    .success
                    .then(|| crate::agent::terminal::TurnHistoryProjection {
                        assistant_content: worker_result.output.clone(),
                        history_commit_len,
                    }),
                history_scope: None,
                provider_outcome: None,
                telemetry: crate::agent::terminal::TurnTerminalTelemetry {
                    summary: if worker_result.success {
                        "session worker completed".to_string()
                    } else {
                        worker_result
                            .error
                            .clone()
                            .unwrap_or_else(|| "session worker failed".to_string())
                    },
                    started_at: provider_started_at,
                    finished_at: chrono::Utc::now(),
                },
                delivery_intent: crate::agent::terminal::TurnDeliveryIntent::Deferred {
                    route: "session_worker_callback".to_string(),
                },
            },
        )
        .await
        .context("commit session-worker terminal before publishing result")?;
    if child_segments.is_some() {
        let event = &terminal_receipt.terminal_event;
        let expected_key = format!("turn:{}:final", manifest.run_id);
        anyhow::ensure!(
            (if manifest.memory_event_recording.enabled {
                event.id > 0
            } else {
                child_grant.is_none() && event.id == 0
            }) && terminal_receipt.terminal_id == manifest.run_id
                && terminal_receipt.usage_settlement.is_none()
                && event.workspace_id == terminal_fabric.workspace_id()
                && event.owner_id == logical_task_scope.owner_id
                && event.run_id == logical_task_scope.run_id
                && event.session_key == logical_task_scope.session_key
                && event.source == logical_task_scope.source
                && event.event_type == "turn.finalized"
                && event.idempotency_key.as_deref() == Some(expected_key.as_str()),
            "worker logical terminal differs from admitted scope"
        );
        let owner = logical_task_scope
            .owner_id
            .as_deref()
            .context("worker terminal owner missing")?;
        if event.id > 0 {
            let stored = terminal_fabric
                .memory()
                .find_owned_message_event(terminal_fabric.workspace_id(), owner, &event.event_id)
                .await?
                .context("worker logical terminal is not durable")?;
            anyhow::ensure!(
                serde_json::to_value(&stored)? == serde_json::to_value(event)?,
                "worker logical terminal differs from durable record"
            );
        }
    }

    if let (Some(segments), Some(grant)) = (child_segments.as_ref(), child_grant.as_ref()) {
        use sha2::Digest;
        let (child, constraints) = grant.scope().gate_inputs();
        let mut evidence = crate::session_worker::protocol::WorkerCompletionEvidence {
            run_id: manifest.run_id.clone(),
            input_event_id: manifest
                .child_source
                .as_ref()
                .context("worker child source missing")?
                .input_event_id
                .clone(),
            terminal_event_id: terminal_receipt.terminal_event.event_id.clone(),
            terminal_event_digest: format!(
                "{:x}",
                sha2::Sha256::digest(serde_json::to_vec(&terminal_receipt.terminal_event)?)
            ),
            recorded_segments: segments.recorded_count().await,
            segments: segments.usage.terminal_references(),
            seal: [0; 32],
        };
        evidence.validate_inventory(worker_result.success)?;
        evidence.seal = constraints.key.hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[
                b"worker-completion-v1",
                child.parent_decision_id.to_string().as_bytes(),
                &evidence.signing_bytes(&worker_result)?,
            ],
        );
        worker_result.completion_evidence = Some(evidence);
    }

    if let (Some(segments), Some(authority)) = (child_segments.as_ref(), ordinary_authority.as_ref()) {
        use sha2::Digest;
        authority.verify()?;
        let terminal = if manifest.memory_event_recording.enabled {
            Some(crate::session_worker::protocol::WorkerSegmentTerminal {
                terminal_id: manifest.run_id.clone(),
                event_id: terminal_receipt.terminal_event.event_id.clone(),
                event_digest: format!(
                    "{:x}",
                    sha2::Sha256::digest(serde_json::to_vec(&terminal_receipt.terminal_event)?)
                ),
            })
        } else {
            None
        };
        let mut evidence = crate::session_worker::protocol::OrdinaryWorkerCompletionEvidence {
            run_id: manifest.run_id.clone(),
            process_id: authority.id().to_string(),
            input_event_id: authority.input().map(|receipt| receipt.event().event_id.clone()),
            recording_enabled: manifest.memory_event_recording.enabled,
            terminal,
            recorded_segments: segments.recorded_count().await,
            confirmed_segments: segments.usage.confirmed_terminal_ids(),
            segments: segments.usage.terminal_references(),
            usage: segments.usage.snapshot(),
            seal: [0; 32],
        };
        evidence.validate_inventory(worker_result.success)?;
        let (child, constraints) = authority.snapshots().scope.gate_inputs();
        evidence.seal = constraints.key.hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[
                b"ordinary-worker-completion-v1",
                child.parent_decision_id.to_string().as_bytes(),
                &evidence.signing_bytes(&worker_result)?,
            ],
        );
        authority.verify()?;
        worker_result.ordinary_completion = Some(evidence);
    }

    if let (Some(segments), Some(authority)) = (child_segments.as_ref(), operator_authority.as_ref()) {
        authority.seal_completion(
            &mut worker_result,
            &terminal_receipt,
            segments.recorded_count().await,
            &segments.usage,
        )?;
    }

    // The result event and the hybrid memory draft persist the worker's reply
    // or error text: redacted like the terminal commit (D-053).
    let event_content = crate::agent::terminal::redact_persisted_text(if worker_result.output.trim().is_empty() {
        worker_result
            .error
            .as_deref()
            .unwrap_or("[session-worker produced no output]")
    } else {
        worker_result.output.as_str()
    });
    SideEffectGate::new(security.as_ref())
        .authorize_resource_operation(
            "session_worker",
            &format!("session_worker:result_event:{}", manifest.run_id),
            ResourceRiskLevel::Low,
            None,
        )
        .map_err(anyhow::Error::msg)?;
    let worker_result_event = match memory_fabric
        .record_worker_result(
            worker_event_scope.clone(),
            crate::agent::terminal::redact_persisted_text(&event_content),
            Some(
                crate::agent::terminal::redact_persisted_json(serde_json::json!({
                    "success": worker_result.success,
                    "error": worker_result.error,
                    "owner_id": manifest.owner_id.as_deref(),
                    "topic_id": manifest.topic_id.as_deref(),
                    "parent_task_id": manifest.parent_task_id.as_deref(),
                    "source_message_event_id": manifest.source_message_event_id.as_deref()
                }))
                .to_string(),
            ),
        )
        .await
    {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::warn!(run_id = %manifest.run_id, "failed to record session-worker result event: {error}");
            None
        }
    };

    record_hybrid_worker_draft_if_needed(
        &manifest,
        &config,
        &memory_fabric,
        &worker_event_scope,
        &worker_result,
        worker_result_event.as_ref(),
        &event_content,
        security.as_ref(),
    )
    .await;

    let completion = child_grant.map(|grant| PendingWorkerCompletion {
        fabric: terminal_fabric.clone(),
        scope: worker_event_scope,
        grant,
    });
    Ok(PreparedWorkerResult {
        _ordinary_lifetime: ordinary_lifetime,
        _operator_lifetime: operator_lifetime,
        operator_completion: operator_authority,
        result: worker_result,
        completion,
        ordinary_completion: ordinary_authority,
    })
}
