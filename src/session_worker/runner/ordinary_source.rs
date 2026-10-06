//! Authenticated ordinary process snapshots; execution admission remains separate.
use super::{Arc, AuthenticatedWorkerTransport, Config, Context, Result, WorkerManifest};
use crate::tools::capability::child::{CapabilityConstraints, ChildCapabilityManifest, VerifiedChildScope};

use crate::runtime::child_source::ordinary_process::OrdinaryProcessSnapshots;

/// Authenticate transport before deriving the ordinary signing key. Restored
/// snapshots are not a grant: input readback, actual executor parity and owned
/// lifetime/terminal settlement are still required before a worker may run.
pub(super) fn restore(
    manifest: &WorkerManifest,
    config: Config,
    transport: &AuthenticatedWorkerTransport,
) -> Result<OrdinaryProcessSnapshots> {
    transport.verify_manifest(manifest)?;
    let source = manifest
        .ordinary_source
        .as_ref()
        .context("ordinary worker source missing")?;
    anyhow::ensure!(
        !config.causal_tree.enabled
            && manifest.child_source.is_none()
            && manifest.capability_manifest.is_none()
            && !manifest.run_id.is_empty()
            && !manifest.session_scope_key.is_empty()
            && manifest.runtime_config_generation_id == Some(source.config_generation.id.0)
            && manifest.runtime_config_source_revision.as_deref()
                == Some(source.config_generation.source_revision.fingerprint_sha256.as_str()),
        "ordinary worker source and configuration lineage differ"
    );
    let workspace = config.workspace_dir.to_string_lossy();
    let key = crate::decision::key::DecisionKey::from_worker_channel(
        &transport.key,
        &workspace,
        &manifest.run_id,
        &manifest.config_generation,
    )?;
    let parent = CapabilityConstraints::from_worker_constraints(&source.parent_constraints, Arc::clone(&key))?;
    anyhow::ensure!(
        parent.authority.workspace == workspace
            && manifest.owner_id.as_deref() == Some(parent.authority.owner.as_str()),
        "ordinary worker source owner or workspace differs"
    );
    let child = ChildCapabilityManifest::from_worker_manifest(&source.capability_manifest, &parent)?;
    let scope = Arc::new(VerifiedChildScope::new(child, &parent)?);
    let catalog = crate::tools::capability::catalog::CapabilityCatalog::restore_process(
        &source.catalog,
        &key,
        parent.catalog_generation,
    )?;
    for reference in &parent.allowed {
        catalog.resolve(std::slice::from_ref(reference))?;
    }
    let generation = crate::config::ConfigGeneration::restore_process(config, &source.config_generation, &key)?;
    let recording = generation.effective.memory.event_recording_config();
    anyhow::ensure!(
        manifest.memory_event_recording.enabled == recording.enabled
            && manifest.memory_event_recording.record_user_messages == recording.record_user_messages
            && manifest.memory_event_recording.record_assistant_messages == recording.record_assistant_messages
            && manifest.memory_event_recording.record_tool_events == recording.record_tool_events,
        "ordinary worker recording policy differs from authenticated configuration"
    );
    let provider_catalog =
        crate::providers::catalog::CatalogView::restore_process(&source.provider_catalog, &key, parent.decision_id)?;
    let selection = &source.provider_selection;
    let resolved = crate::providers::selection::check_with(
        &generation.effective,
        &provider_catalog,
        &selection.provider,
        &selection.model,
        selection.credential.clone(),
        &crate::providers::catalog::Requirement::chat(),
        crate::providers::catalog::UnknownPolicy::Invocation,
    )?;
    let route = crate::providers::selection::route_target(&provider_catalog, &selection.provider, &selection.model)?;
    let root = matches!(
        resolved.credential().reference(),
        crate::providers::credentials::CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root)
    );
    let credentials = super::worker_credentials(manifest, &generation.effective);
    anyhow::ensure!(
        crate::providers::ProviderId::of(resolved.provider_label())
            == crate::providers::ProviderId::of(&manifest.provider_name)
            && resolved.model() == manifest.model
            && resolved.credential().api_key() == credentials.sealed_key
            && route.map(|route| route.hint.as_str()) == manifest.route_hint.as_deref()
            && manifest.direct_uses_root_connection == (route.is_none() && root)
            && manifest.route_uses_root_connection == (route.is_some() && root)
            && manifest.route_source_provider.as_deref() == route.map(|_| selection.provider.as_str())
            && manifest.route_credential.as_ref() == route.map(|_| resolved.credential().reference()),
        "ordinary worker provider differs from authenticated selection"
    );
    let parent =
        crate::runtime::child_source::parent::ParentSource::restore_ordinary_process(manifest, &scope, &generation)?;
    Ok(OrdinaryProcessSnapshots {
        scope,
        catalog,
        generation,
        provider_catalog,
        parent,
    })
}
