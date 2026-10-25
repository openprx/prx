//! Explicit operator publication; no parent model decision or durable grant.
use super::{
    Arc, BTreeSet, CapabilityAvailabilityLevel, CapabilityRef, CatalogScope, ConfigGeneration, Context,
    DecisionBackendFactory, DecisionKey, FactoryDeps, Memory, SecurityPolicy, Tool, ToolExecutionService,
    TurnDecisionService, TurnLoopRuntime, TurnSourceProof, TurnSurfaceKind, pinned_policy_version,
};

/// Frozen upper bound for an operator-launched child. The child still needs its
/// own source/lifetime admission and per-call policy/approval checks.
pub(crate) struct OperatorChildPublication {
    service: Arc<TurnDecisionService>,
    source: super::super::super::turn_source::OwnedTurnSourceProof,
    allowed: BTreeSet<CapabilityRef>,
}

impl OperatorChildPublication {
    pub(crate) fn capture(
        service: Arc<TurnDecisionService>,
        source: &dyn TurnSourceProof,
        generation: &Arc<ConfigGeneration>,
        memory: &Arc<dyn Memory>,
        security: &Arc<SecurityPolicy>,
    ) -> anyhow::Result<Self> {
        service.verify_operator_spawn_source(source, generation, memory, security)?;
        let allowed = service
            .catalog_view()
            .iter_entries()
            .filter(|entry| entry.availability.level >= CapabilityAvailabilityLevel::Configured)
            .filter(|entry| {
                crate::tools::execution::decide_tool_execution(
                    security,
                    entry.policy_tool_name(),
                    source.policy_sender(),
                    source.policy_channel(),
                    source.policy_chat_type(),
                ) != crate::tools::execution::ToolExecutionDecision::Deny
            })
            .map(|entry| CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            })
            .collect();
        let publication = Self {
            service,
            source: super::super::super::turn_source::OwnedTurnSourceProof::capture(source),
            allowed,
        };
        publication.verify(source, generation, memory, security)?;
        Ok(publication)
    }

    pub(crate) fn verify(
        &self,
        source: &dyn TurnSourceProof,
        generation: &Arc<ConfigGeneration>,
        memory: &Arc<dyn Memory>,
        security: &Arc<SecurityPolicy>,
    ) -> anyhow::Result<()> {
        self.service
            .verify_operator_spawn_source(source, generation, memory, security)?;
        anyhow::ensure!(
            source.owner_id() == self.source.owner_id()
                && source.session_key() == self.source.session_key()
                && source.run_id() == self.source.run_id()
                && source.source_event_id() == self.source.source_event_id()
                && source.routing_input() == self.source.routing_input()
                && source.policy_chat_type() == self.source.policy_chat_type(),
            "operator capability publication belongs to another source"
        );
        let view = self.service.catalog_view();
        for reference in &self.allowed {
            let entry = view
                .entry_any(&reference.id)
                .context("operator capability disappeared from publication")?;
            anyhow::ensure!(
                view.contains(&entry.id)
                    && entry.fingerprint == reference.fingerprint
                    && entry.generation == reference.generation
                    && entry.availability.level >= CapabilityAvailabilityLevel::Configured
                    && crate::tools::execution::decide_tool_execution(
                        security,
                        entry.policy_tool_name(),
                        source.policy_sender(),
                        source.policy_channel(),
                        source.policy_chat_type(),
                    ) != crate::tools::execution::ToolExecutionDecision::Deny,
                "operator capability publication changed or became denied"
            );
        }
        Ok(())
    }
}

impl OperatorChildPublication {
    pub(crate) fn loop_runtime(&self) -> anyhow::Result<&TurnLoopRuntime> {
        self.service
            .loop_runtime
            .as_ref()
            .context("operator loop runtime missing")
    }

    pub(crate) fn task_service(
        &self,
        task: Arc<crate::runtime::operator_spawn::task::OperatorTaskAuthority>,
        registry: Arc<Vec<Box<dyn Tool>>>,
    ) -> anyhow::Result<Arc<TurnDecisionService>> {
        anyhow::ensure!(
            std::ptr::eq(self, task.publication()),
            "operator task publication differs"
        );
        task.verify()?;
        let parent = &self.service;
        anyhow::ensure!(
            Arc::ptr_eq(&registry, &parent.registry),
            "operator task registry differs from source"
        );
        let runtime = parent
            .loop_runtime
            .clone()
            .context("operator task loop runtime missing")?;
        let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
            Arc::clone(&registry),
            Arc::new(crate::tools::SecurityEffectPolicy::new(Arc::clone(&parent.security))),
            Arc::new(crate::tools::DenyApprovalStrategy),
            Arc::new(crate::tools::AdapterOwnedPreparation),
            Arc::new(crate::tools::TracingToolExecutionAudit),
        ));
        let mut policy = parent.publication_policy.clone();
        let view = parent.catalog.view(&CatalogScope::default());
        let mut names = BTreeSet::new();
        for entry in view.iter_entries() {
            let reference = CapabilityRef {
                id: entry.id.clone(),
                fingerprint: entry.fingerprint,
                generation: entry.generation,
            };
            if self.allowed.contains(&reference) {
                names.insert(entry.public_name.clone());
            } else {
                // Explicit exclusions also prevent root aliases from admitting
                // a sibling Skill/Node outside the frozen capability set.
                policy.excluded_names.insert(entry.public_name.clone());
            }
        }
        policy.allowed_tools = Some(names);
        parent.catalog.check_parity(&execution)?;
        let policy_version = pinned_policy_version(
            &parent.generation.effective,
            &parent.security,
            &policy,
            &parent.state_key,
        )?;
        let factory = DecisionBackendFactory::from_config(
            &parent.generation.effective,
            FactoryDeps {
                catalog: &parent.provider_catalog,
                key: parent.durable_key.then(|| Arc::clone(&parent.state_key)),
                transports: &crate::decision::compatible_http::HttpDecisionTransports,
                config_generation: parent.generation.id.0,
                events: Arc::clone(&parent.events),
            },
        )?;
        let service = Arc::new(TurnDecisionService {
            child_recovery: Default::default(),
            recursive_allowances: Default::default(),
            memory_loader: None,
            loop_runtime: Some(runtime),
            publication_policy: policy,
            operator_task: Some(Arc::clone(&task)),
            operator_process: None,
            ordinary_parent: None,
            ordinary_money: None,
            ordinary_task: None,
            ordinary_process: None,
            parent_grant: None,
            service_id: uuid::Uuid::now_v7(),
            surface: TurnSurfaceKind::Worker,
            generation: Arc::clone(&parent.generation),
            catalog: Arc::clone(&parent.catalog),
            provider_catalog: parent.provider_catalog.clone(),
            factory,
            events: Arc::clone(&parent.events),
            state_key: Arc::clone(&parent.state_key),
            policy_version,
            durable_key: parent.durable_key,
            recordable: parent.recordable,
            registry,
            execution,
            memory: Arc::clone(&parent.memory),
            security: Arc::clone(&parent.security),
            store: Arc::clone(&parent.store),
            durable_events: parent.durable_events.clone(),
        });
        service.resolve_agent_model(task.as_ref())?;
        task.verify()?;
        Ok(service)
    }
}

impl OperatorChildPublication {
    pub(crate) fn bind_process_publication(
        &self,
        id: uuid::Uuid,
        key: &DecisionKey,
    ) -> anyhow::Result<crate::session_worker::protocol::WorkerOperatorPublication> {
        let parent = &self.service;
        self.verify(&self.source, &parent.generation, &parent.memory, &parent.security)?;
        let mut wire = crate::session_worker::protocol::WorkerOperatorPublication {
            publication_id: id.to_string(),
            owner: self.source.owner_id().to_owned(),
            workspace: self.source.workspace_id().to_owned(),
            level: parent.security.autonomy,
            max_cost_cents: parent.security.max_cost_per_day_cents,
            allowed: self
                .allowed
                .iter()
                .map(
                    |reference| crate::tools::capability::child::wire::WorkerChildCapabilityRef {
                        id: reference.id.as_str().to_owned(),
                        fingerprint: reference.fingerprint.0,
                        generation: reference.generation.0,
                    },
                )
                .collect(),
            catalog_generation: parent.catalog.generation().0,
            seal: [0; 32],
        };
        wire.sign(key)?;
        wire.verify(key, &parent.catalog)?;
        self.verify(&self.source, &parent.generation, &parent.memory, &parent.security)?;
        Ok(wire)
    }
}

impl OperatorChildPublication {
    pub(crate) fn bind_process_snapshots(
        &self,
        id: uuid::Uuid,
        key: &DecisionKey,
        input: Option<crate::session_worker::protocol::WorkerRecordedInput>,
        selection: crate::session_worker::protocol::WorkerProviderSelection,
        attempt_id: Option<String>,
        lease_epoch: Option<i64>,
    ) -> anyhow::Result<crate::session_worker::protocol::WorkerOperatorSource> {
        Ok(crate::session_worker::protocol::WorkerOperatorSource {
            publication: self.bind_process_publication(id, key)?,
            input,
            provider_selection: selection,
            attempt_id,
            lease_epoch,
            operator_source: crate::runtime::child_source::parent::ParentSource::capture(&self.source)
                .bind_operator_process(id, key)?,
            provider_catalog: self.service.provider_catalog.bind_operator_process(key, id)?,
            config_generation: self.service.generation.bind_process(key)?,
            catalog: self.service.catalog.bind_process(key)?,
        })
    }
}
