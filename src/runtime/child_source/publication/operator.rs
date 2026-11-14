//! Explicit operator publication; no parent model decision or durable grant.
use crate::capability::CapabilityAvailabilityLevel;
use crate::config::ConfigGeneration;
use crate::decision_engine::prediction::factory::DecisionBackendFactory;
use crate::decision_engine::prediction::factory::FactoryDeps;
use crate::memory::Memory;
use crate::runtime::turn_publication::PublicationLoopRuntime;
use crate::runtime::turn_publication::TurnRuntimeService;
use crate::runtime::turn_publication::service::pinned_policy_version;
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::SecurityPolicy;
use crate::security::runtime_key::DecisionKey;
use crate::tools::capability::catalog::CatalogScope;
use crate::tools::capability::contract::CapabilityRef;
use crate::tools::capability::turn_state::TurnSurfaceKind;
use crate::tools::execution::ToolExecutionService;
use crate::tools::traits::Tool;
use anyhow::Context;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Frozen upper bound for an operator-launched child below Full (a Full task
/// is not limited by it, R51). The child still needs its own source/lifetime
/// admission and per-call policy/approval checks.
pub(crate) struct OperatorChildPublication {
    service: Arc<TurnRuntimeService>,
    source: crate::runtime::turn_source::OwnedTurnSourceProof,
    authority: crate::runtime::child_source::ParentAuthority,
}

impl OperatorChildPublication {
    pub(crate) fn capture(
        service: Arc<TurnRuntimeService>,
        runtime: Arc<crate::runtime::turn_context::TurnRuntimeHolder>,
        source: &dyn TurnSourceProof,
        generation: &Arc<ConfigGeneration>,
        memory: &Arc<dyn Memory>,
        security: &Arc<SecurityPolicy>,
    ) -> anyhow::Result<Self> {
        service.verify_operator_spawn_source(source, generation, memory, security)?;
        // R26: the operator child is funded from the publication's runtime
        // holder, which pins the same generation and security policy.
        anyhow::ensure!(
            Arc::ptr_eq(&runtime, &service.runtime),
            "operator runtime holder belongs to another publication"
        );
        let allowed: BTreeSet<_> = service
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
        let remaining = runtime.workspace_remaining_micro_usd()?;
        let authority = crate::runtime::child_source::ParentAuthority::for_run(
            source.run_id(),
            allowed.clone(),
            crate::tools::capability::child::AuthorityEnvelope {
                owner: source.owner_id().to_owned(),
                workspace: source.workspace_id().to_owned(),
                level: security.autonomy,
                max_cost_cents: security.max_cost_per_day_cents,
                remaining_depth: u8::try_from(allowed.len()).unwrap_or(u8::MAX),
                delegation: None,
            },
            *service.catalog.generation(),
            Arc::clone(&service.state_key),
            Arc::new(crate::runtime::child_source::recursive_allowance::RecursiveAllowance::new(remaining)),
        );
        let publication = Self::from_authority(service, source, authority);
        publication.verify(source, generation, memory, security)?;
        Ok(publication)
    }

    fn from_authority(
        service: Arc<TurnRuntimeService>,
        source: &dyn TurnSourceProof,
        authority: crate::runtime::child_source::ParentAuthority,
    ) -> Self {
        Self {
            service,
            source: crate::runtime::turn_source::OwnedTurnSourceProof::capture(source),
            authority,
        }
    }

    pub(crate) const fn parent_authority(&self) -> &crate::runtime::child_source::ParentAuthority {
        &self.authority
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
        for reference in &self.authority.constraints.allowed {
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
    pub(crate) fn loop_runtime(&self) -> anyhow::Result<&PublicationLoopRuntime> {
        self.service
            .loop_runtime
            .as_ref()
            .context("operator loop runtime missing")
    }

    pub(crate) fn task_service(
        &self,
        task: Arc<crate::runtime::operator_spawn::task::OperatorTaskAuthority>,
        registry: Arc<Vec<Box<dyn Tool>>>,
    ) -> anyhow::Result<Arc<TurnRuntimeService>> {
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
        // The frozen capability set is a membership limit. Full has no
        // permission limits (R51/OV-4): a Full task publishes under the
        // parent's surface policy; parity, budget and per-call policy stay.
        if parent.security.autonomy != crate::security::AutonomyLevel::Full {
            let view = parent.catalog.view(&CatalogScope::default());
            let mut names = BTreeSet::new();
            for entry in view.iter_entries() {
                let reference = CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                };
                if self.authority.constraints.allowed.contains(&reference) {
                    names.insert(entry.public_name.clone());
                } else {
                    // Explicit exclusions also prevent root aliases from admitting
                    // a sibling Skill/Node outside the frozen capability set.
                    policy.excluded_names.insert(entry.public_name.clone());
                }
            }
            policy.allowed_tools = Some(names);
        }
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
                key: Some(Arc::clone(&parent.state_key)),
                transports: &crate::decision_engine::prediction::compatible_http::HttpDecisionTransports,
                config_generation: parent.generation.id.0,
                events: Arc::clone(&parent.events),
            },
        )?;
        let service = Arc::new(TurnRuntimeService {
            runtime: Arc::new(crate::runtime::turn_context::TurnRuntimeHolder::new(
                Arc::new(crate::runtime::ingress_recovery::RecoveryScope {
                    surface: TurnSurfaceKind::Worker,
                    ..parent.runtime.recovery_scope().as_ref().clone()
                }),
                Some(task.allowance().money()),
                &parent.generation.effective,
                &parent.security,
            )),
            // R9/R26: an enabled engine analyses the child's own turns on
            // the child's runtime context and accounting scope.
            advisory: crate::runtime::decision_advisory::DecisionAdvisory::for_publication(
                &parent.generation.effective,
                FactoryDeps {
                    catalog: &parent.provider_catalog,
                    key: Some(Arc::clone(&parent.state_key)),
                    transports: &crate::decision_engine::prediction::compatible_http::HttpDecisionTransports,
                    config_generation: parent.generation.id.0,
                    events: Arc::clone(&parent.events),
                },
            )?,
            memory_loader: None,
            loop_runtime: Some(runtime),
            publication_policy: policy,
            operator_task: Some(Arc::clone(&task)),
            operator_process: None,
            ordinary_parent: None,
            ordinary_task: None,
            ordinary_process: None,
            service_id: uuid::Uuid::now_v7(),
            surface: TurnSurfaceKind::Worker,
            generation: Arc::clone(&parent.generation),
            catalog: Arc::clone(&parent.catalog),
            provider_catalog: parent.provider_catalog.clone(),
            factory,
            events: Arc::clone(&parent.events),
            state_key: Arc::clone(&parent.state_key),
            policy_version,
            recordable: parent.recordable,
            registry,
            execution,
            memory: Arc::clone(&parent.memory),
            security: Arc::clone(&parent.security),
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
                .authority
                .constraints
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

impl TurnRuntimeService {
    /// Local operator commands retain the actual Interactive publication.
    /// This validates source provenance without inventing a model decision.
    pub(crate) fn verify_operator_spawn_source(
        &self,
        source: &dyn TurnSourceProof,
        generation: &Arc<ConfigGeneration>,
        memory: &Arc<dyn Memory>,
        security: &Arc<SecurityPolicy>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.surface == TurnSurfaceKind::Interactive
                && Arc::ptr_eq(&self.generation, generation)
                && Arc::ptr_eq(&self.memory, memory)
                && Arc::ptr_eq(&self.security, security)
                && self.child_scope().is_none()
                && source.policy_channel() == "terminal"
                && source.policy_sender() == "local-user",
            "operator spawn differs from its actual runtime publication"
        );
        crate::runtime::turn_context::validate_agent_turn_origin(&self.generation, self.surface, source)?;
        self.check_live_parity()?;
        let entry = self
            .catalog
            .entry_for_public_name("sessions_spawn")
            .context("operator spawn capability is absent from its publication")?;
        anyhow::ensure!(
            self.catalog_view().contains(&entry.id)
                && entry.availability.level >= CapabilityAvailabilityLevel::Configured
                && crate::tools::execution::decide_tool_execution(
                    &self.security,
                    entry.policy_tool_name(),
                    source.policy_sender(),
                    source.policy_channel(),
                    source.policy_chat_type(),
                ) != crate::tools::execution::ToolExecutionDecision::Deny,
            "operator spawn capability is unavailable or denied by publication policy"
        );
        // Ask still requires the explicit command's resource-operation grant
        // at the existing spawn gate; source validation never mints approval.
        Ok(())
    }
}
