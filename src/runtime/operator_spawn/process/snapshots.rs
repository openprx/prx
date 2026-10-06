//! Restore signed operator snapshots for both worker admission and parent verification.
use super::{DecisionKey, OperatorProcessSnapshots};
use anyhow::Context;

impl OperatorProcessSnapshots {
    pub(crate) fn restore(
        manifest: &crate::session_worker::protocol::WorkerManifest,
        config: crate::config::Config,
        key: std::sync::Arc<DecisionKey>,
    ) -> anyhow::Result<Self> {
        let source = manifest
            .operator_source
            .as_ref()
            .context("operator worker source missing")?;
        anyhow::ensure!(
            manifest.child_source.is_none()
                && manifest.capability_manifest.is_none()
                && manifest.ordinary_source.is_none()
                && !manifest.run_id.is_empty()
                && !manifest.session_scope_key.is_empty()
                && manifest.runtime_config_generation_id == Some(source.config_generation.id.0)
                && manifest.runtime_config_source_revision.as_deref()
                    == Some(source.config_generation.source_revision.fingerprint_sha256.as_str()),
            "operator worker source and configuration lineage differ"
        );
        let workspace = config.workspace_dir.to_string_lossy();
        let catalog = crate::tools::capability::catalog::CapabilityCatalog::restore_process(
            &source.catalog,
            &key,
            crate::causal_tree::branch::CatalogGeneration(source.publication.catalog_generation),
        )?;
        let publication = source.publication.verify(&key, &catalog)?;
        anyhow::ensure!(
            publication.workspace() == workspace && manifest.owner_id.as_deref() == Some(publication.owner()),
            "operator worker owner or workspace differs"
        );
        let generation = crate::config::ConfigGeneration::restore_process(config, &source.config_generation, &key)?;
        let recording = generation.effective.memory.event_recording_config();
        anyhow::ensure!(
            manifest.memory_event_recording.enabled == recording.enabled
                && manifest.memory_event_recording.record_user_messages == recording.record_user_messages
                && manifest.memory_event_recording.record_assistant_messages == recording.record_assistant_messages
                && manifest.memory_event_recording.record_tool_events == recording.record_tool_events,
            "operator worker recording policy differs from authenticated configuration"
        );
        let provider_catalog = crate::providers::catalog::CatalogView::restore_operator_process(
            &source.provider_catalog,
            &key,
            publication.id(),
        )?;
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
        let route =
            crate::providers::selection::route_target(&provider_catalog, &selection.provider, &selection.model)?
                .cloned();
        let root = matches!(
            resolved.credential().reference(),
            crate::providers::credentials::CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root)
        );
        anyhow::ensure!(
            crate::providers::ProviderId::of(resolved.provider_label())
                == crate::providers::ProviderId::of(&manifest.provider_name)
                && resolved.model() == manifest.model
                && resolved.credential().api_key() == manifest.api_key.as_deref()
                && route.as_ref().map(|route| route.hint.as_str()) == manifest.route_hint.as_deref()
                && manifest.direct_uses_root_connection == (route.is_none() && root)
                && manifest.route_uses_root_connection == (route.is_some() && root)
                && manifest.route_source_provider.as_deref() == route.as_ref().map(|_| selection.provider.as_str())
                && manifest.route_credential.as_ref() == route.as_ref().map(|_| resolved.credential().reference()),
            "operator worker provider differs from authenticated selection"
        );
        let parent =
            crate::runtime::child_source::parent::ParentSource::restore_operator_process(manifest, &publication, &key)?;
        let security = crate::security::SecurityPolicy::from_config(
            &generation.effective.autonomy,
            &generation.effective.workspace_dir,
        );
        publication.verify_policy(&security, &parent, &catalog)?;
        Ok(Self {
            publication,
            key,
            catalog,
            generation,
            provider_catalog,
            parent,
            selection: selection.clone(),
            resolved,
            route,
        })
    }
}
