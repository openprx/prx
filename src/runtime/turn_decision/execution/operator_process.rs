//! Bind an authenticated operator worker to its actual execution service.
use super::{
    Arc, BTreeSet, CapabilityRef, CatalogScope, CausalEventSink, DecisionBackendFactory, DecisionKey, FactoryDeps,
    StoreMode, TurnDecisionService, TurnLoopRuntime, TurnServiceBuild, TurnServiceBuildError, TurnSurfaceKind,
    pinned_policy_version,
};

impl TurnDecisionService {
    pub(crate) async fn operator_process_service(
        authority: Arc<crate::runtime::child_source::operator_process::OperatorProcessAuthority>,
        mut build: TurnServiceBuild<'_>,
        runtime: TurnLoopRuntime,
    ) -> anyhow::Result<Arc<Self>> {
        authority.verify()?;
        let snapshots = authority.snapshots();
        anyhow::ensure!(
            build.parent_grant.is_none()
                && Arc::ptr_eq(&build.generation, &snapshots.generation)
                && Arc::ptr_eq(&build.memory, &authority.source_fabric().memory())
                && build.catalog_sources.config_generation_id == snapshots.generation.id.0.to_string(),
            "operator worker service differs from source"
        );
        snapshots
            .publication
            .verify_policy(&build.security, &snapshots.parent, &snapshots.catalog)?;
        let view = snapshots.catalog.view(&CatalogScope::default());
        let mut names = BTreeSet::new();
        for entry in view.iter_entries() {
            let reference = CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            };
            if snapshots.publication.allowed().contains(&reference) {
                anyhow::ensure!(
                    build
                        .surface_policy
                        .permits(&entry.public_name, &entry.root, &entry.target)
                        && !build.surface_policy.excluded_names.contains(&entry.public_name)
                        && !build.surface_policy.excluded_names.contains(&entry.root),
                    "operator worker capability exceeds actual surface policy"
                );
                names.insert(entry.public_name.clone());
            } else {
                build.surface_policy.excluded_names.insert(entry.public_name.clone());
            }
        }
        build.surface_policy.allowed_tools = Some(names);
        snapshots
            .catalog
            .check_published_parity(&build.execution, snapshots.publication.allowed())?;
        let config = &snapshots.generation.effective;
        anyhow::ensure!(
            !config.causal_tree.enabled
                || (crate::memory::serves_tool_execution_ledger(build.memory.name())
                    && config.memory.events.record_user_messages),
            "operator worker decision recording is unavailable"
        );
        let workspace_key = super::super::store_preparation::load_key(Arc::clone(&build.generation)).await?;
        let durable_key = config.causal_tree.enabled && workspace_key.is_some();
        let durable_events = if durable_key {
            Some(Arc::new(crate::runtime::causal_events::DurableCausalEventSink::new(
                &build.generation,
                Arc::clone(&build.memory),
                Arc::clone(&build.events),
            )?))
        } else {
            None
        };
        let events: Arc<dyn CausalEventSink> = durable_events.as_ref().map_or_else(
            || Arc::clone(&build.events),
            |events| Arc::clone(events) as Arc<dyn CausalEventSink>,
        );
        let store = super::super::store_preparation::open_store(
            Arc::clone(&build.generation),
            Arc::clone(&events),
            durable_events.clone(),
            durable_key,
        )
        .await?;
        let state_key = workspace_key.clone().unwrap_or_else(DecisionKey::ephemeral);
        let policy_version = pinned_policy_version(config, &build.security, &build.surface_policy, &state_key)
            .map_err(TurnServiceBuildError::PolicyFingerprint)?;

        let factory = DecisionBackendFactory::from_config(
            config,
            FactoryDeps {
                catalog: &snapshots.provider_catalog,
                key: workspace_key,
                transports: build.transports,
                config_generation: snapshots.generation.id.0,
                events: Arc::clone(&events),
            },
        )?;
        let recordable = durable_key && store.mode() == StoreMode::ReadWrite;
        let service = Arc::new(Self {
            child_recovery: Default::default(),
            recursive_allowances: Default::default(),
            memory_loader: None,
            loop_runtime: Some(runtime),
            publication_policy: build.surface_policy,
            operator_process: Some(Arc::clone(&authority)),
            operator_task: None,
            ordinary_parent: None,
            ordinary_task: None,
            ordinary_process: None,
            parent_grant: None,
            service_id: uuid::Uuid::now_v7(),
            surface: TurnSurfaceKind::Worker,
            generation: Arc::clone(&snapshots.generation),
            catalog: Arc::clone(&snapshots.catalog),
            provider_catalog: snapshots.provider_catalog.as_ref().clone(),
            factory,
            events,
            state_key,
            policy_version,
            durable_key,
            recordable,
            registry: build.registry,
            execution: build.execution,
            memory: build.memory,
            security: build.security,
            store,
            durable_events,
        });
        service.resolve_agent_model(&authority.source_proof()?)?;
        service.check_live_parity()?;
        Ok(service)
    }
}
