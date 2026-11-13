//! Child publications captured from a live parent publication: ordinary and
//! operator children, the ordinary process launch and the execution-policy
//! proof carried into an issued child grant.
mod operator;
pub(crate) use operator::OperatorChildPublication;
mod ordinary_process;
pub(crate) use ordinary_process::OrdinaryProcessLaunch;

use crate::decision_engine::prediction::factory::{DecisionBackendFactory, FactoryDeps};
use crate::runtime::turn_context::validate_agent_turn_origin;
use crate::runtime::turn_decision::{TurnDecisionContext, TurnDecisionService, pinned_policy_version};
use crate::runtime::turn_publication::PublicationLoopRuntime;
use crate::runtime::turn_source::TurnSourceProof;
use crate::security::SecurityPolicy;
use crate::tools::capability::catalog::CatalogScope;
use crate::tools::capability::contract::CapabilityRef;
use crate::tools::capability::turn_state::TurnSurfaceKind;
use crate::tools::execution::ToolExecutionService;
use crate::tools::traits::Tool;
use anyhow::Context;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Proof that child execution retains the exact policy of an admitted service.
/// Only this module constructs its fields, and only from an admitted service.
pub(crate) struct ChildExecutionPolicy {
    security: Arc<SecurityPolicy>,
}

impl ChildExecutionPolicy {
    /// The exact policy of the admitted service that issues the child grant.
    pub(in crate::runtime) fn of_service(service: &TurnDecisionService) -> Self {
        Self {
            security: Arc::clone(&service.security),
        }
    }

    pub(super) fn matches(&self, security: &Arc<SecurityPolicy>) -> bool {
        Arc::ptr_eq(&self.security, security)
    }
}

/// Read-only provenance for ordinary disabled children. Retaining the actual
/// service preserves its catalog, PMM and runtime extension generation.
#[derive(Clone)]
pub(crate) struct OrdinaryChildPublication(Arc<OrdinaryChildPublicationInner>);

struct OrdinaryChildPublicationInner {
    context: Arc<TurnDecisionContext>,
    authority: crate::runtime::child_source::ParentAuthority,
    scope: Arc<crate::tools::capability::child::VerifiedChildScope>,
    service: Arc<TurnDecisionService>,
    source: crate::runtime::turn_source::OwnedTurnSourceProof,
}

impl std::fmt::Debug for OrdinaryChildPublication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OrdinaryChildPublication")
    }
}

impl PartialEq for OrdinaryChildPublication {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for OrdinaryChildPublication {}

impl OrdinaryChildPublication {
    pub(crate) fn parent_authority(&self) -> &crate::runtime::child_source::ParentAuthority {
        &self.0.authority
    }

    pub(crate) fn intercepts_writes(&self) -> bool {
        self.0.source.intercepts_writes()
    }

    pub(crate) fn dispatch_pin(
        &self,
        owner: &str,
        workspace: &str,
    ) -> Arc<crate::tools::capability::child::ChildDispatchPin> {
        crate::tools::capability::child::ChildDispatchPin::for_scope(
            &self.0.scope,
            Arc::clone(&self.0.service.catalog),
            owner,
            workspace,
        )
    }

    pub(in crate::runtime) fn capture(
        service: &Arc<TurnDecisionService>,
        context: &Arc<TurnDecisionContext>,
        source: &dyn TurnSourceProof,
        authority: crate::runtime::child_source::ParentAuthority,
        requested: &[CapabilityRef],
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !service.generation.effective.causal_tree.enabled,
            "ordinary publication requires disabled plane"
        );
        let manifest = crate::tools::capability::child::ChildCapabilityManifest::derive_strict_subset(
            &authority.constraints,
            requested,
        )?;
        let scope = Arc::new(crate::tools::capability::child::VerifiedChildScope::new(
            manifest,
            &authority.constraints,
        )?);
        Ok(Self(Arc::new(OrdinaryChildPublicationInner {
            context: Arc::clone(context),
            authority,
            scope,
            service: Arc::clone(service),
            source: crate::runtime::turn_source::OwnedTurnSourceProof::capture(source),
        })))
    }

    pub(crate) fn verify_caller(&self, caller: &crate::tools::ToolExecutionContext) -> anyhow::Result<()> {
        let source = &self.0.source;
        let envelope = &caller.envelope;
        validate_agent_turn_origin(&self.0.service.generation, self.0.service.surface, source)?;
        anyhow::ensure!(
            !self.0.service.generation.effective.causal_tree.enabled
                && !self.0.context.execution_closed.load(Ordering::Acquire)
                && caller.execution_trace.as_ref() == self.0.context.execution_trace.get()
                && envelope.workspace_id == source.workspace_id()
                && envelope.resolved_owner_id() == source.owner_id()
                && envelope.session_key == source.session_key()
                && envelope.run_id.as_deref() == Some(source.run_id())
                && envelope.task_id.as_deref() == source.task_id()
                && envelope.source_message_event_id.as_deref() == source.source_event_id()
                && envelope.config_generation_id == Some(source.config_generation_id())
                && envelope.config_source_revision.as_deref() == Some(source.config_source_revision())
                && envelope.channel.as_deref() == Some(source.policy_channel())
                && envelope.sender.as_deref() == Some(source.policy_sender())
                && caller.chat_type == source.policy_chat_type()
                && caller.execution_trace.as_ref().is_some_and(|trace| !trace.is_sealed()),
            "ordinary child publication differs from its live caller"
        );
        self.0.scope.validate_runtime(
            &self.0.service.state_key,
            source.owner_id(),
            source.workspace_id(),
            *self.0.service.catalog.generation(),
        )?;
        self.0.service.check_live_parity()?;
        Ok(())
    }

    /// Check retained authority without accepting caller-supplied identities.
    /// A child cannot outlive the parent execution that selected its manifest.
    pub(in crate::runtime) fn verify_live(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.0.service.generation.effective.causal_tree.enabled
                && !self.0.context.execution_closed.load(Ordering::Acquire)
                && self
                    .0
                    .context
                    .execution_trace
                    .get()
                    .is_some_and(|trace| !trace.is_sealed()),
            "ordinary child parent execution is closed"
        );
        self.0.scope.validate_runtime(
            &self.0.service.state_key,
            self.0.source.owner_id(),
            self.0.source.workspace_id(),
            *self.0.service.catalog.generation(),
        )?;
        self.0.service.check_live_parity()?;
        Ok(())
    }

    fn allocate_child_budget(&self) -> anyhow::Result<(u32, Arc<super::recursive_allowance::RecursiveAllowance>)> {
        self.0
            .authority
            .allowance
            .carve_ordinary_child(self.scope().gate_inputs().0.authority.max_cost_cents)
    }

    /// Derive an in-process worker from the actual parent publication. It
    /// borrows the parent's complete catalog and keeps its exact selected
    /// scope; it never creates a durable grant or loads a workspace key.
    pub(crate) fn child_service(
        &self,
        caller: &crate::tools::ToolExecutionContext,
        allowed_tools: Option<BTreeSet<String>>,
    ) -> anyhow::Result<Arc<TurnDecisionService>> {
        self.verify_caller(caller)?;
        self.derive_child_service(allowed_tools, None, None, &[])
    }

    pub(crate) fn task_service(
        &self,
        task: Arc<super::ordinary_task::OrdinaryTaskAuthority>,
        registry: Arc<Vec<Box<dyn Tool>>>,
        allowed_tools: Option<BTreeSet<String>>,
        excluded_tools: &[String],
    ) -> anyhow::Result<Arc<TurnDecisionService>> {
        anyhow::ensure!(self == task.publication(), "ordinary task publication differs");
        task.verify()?;
        self.derive_child_service(allowed_tools, Some(task), Some(registry), excluded_tools)
    }

    fn derive_child_service(
        &self,
        allowed_tools: Option<BTreeSet<String>>,
        task: Option<Arc<super::ordinary_task::OrdinaryTaskAuthority>>,
        task_registry: Option<Arc<Vec<Box<dyn Tool>>>>,
        excluded_tools: &[String],
    ) -> anyhow::Result<Arc<TurnDecisionService>> {
        let runtime = self.loop_runtime()?.clone();
        let parent = &self.0.service;
        let (registry, execution) = task_registry.map_or_else(
            || (Arc::clone(&parent.registry), Arc::clone(&parent.execution)),
            |registry| {
                let execution = Arc::new(ToolExecutionService::from_shared_boxed_registry(
                    Arc::clone(&registry),
                    Arc::new(crate::tools::SecurityEffectPolicy::new(Arc::clone(&parent.security))),
                    Arc::new(crate::tools::DenyApprovalStrategy),
                    Arc::new(crate::tools::AdapterOwnedPreparation),
                    Arc::new(crate::tools::TracingToolExecutionAudit),
                ));
                (registry, execution)
            },
        );
        let mut policy = parent.publication_policy.clone();
        policy.excluded_names.extend(excluded_tools.iter().cloned());
        if let Some(allowed_tools) = allowed_tools {
            policy.allowed_tools = Some(allowed_tools);
        }
        let parent_view = parent.catalog_view();
        let view = parent.catalog.view(&CatalogScope::default()).with_surface(&policy);
        for reference in &self.scope().gate_inputs().0.allowed {
            let entry = view
                .entry_any(&reference.id)
                .context("ordinary child capability missing")?;
            anyhow::ensure!(
                !view.surface_excludes(entry) && !parent_view.surface_excludes(entry),
                "ordinary child capability exceeds publication policy"
            );
        }
        parent.catalog.check_child_parity(&execution, self.scope())?;
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
                key: None,
                transports: &crate::decision_engine::prediction::compatible_http::HttpDecisionTransports,
                config_generation: parent.generation.id.0,
                events: Arc::clone(&parent.events),
            },
        )?
        .with_unproven_budget_gate();
        Ok(Arc::new(TurnDecisionService {
            runtime: Arc::new(crate::runtime::turn_context::TurnRuntimeHolder::new(
                Arc::new(crate::runtime::ingress_recovery::RecoveryScope::unrecorded(
                    Arc::clone(&parent.memory),
                    parent.generation.effective.workspace_dir.clone(),
                    TurnSurfaceKind::Worker,
                )),
                Some(self.allocate_child_budget()?.1.money()),
                None,
                &parent.generation.effective,
                &parent.security,
            )),
            advisory: crate::runtime::decision_advisory::DecisionAdvisory::new(
                Arc::clone(&parent.generation.effective),
                None,
            ),
            child_recovery: Default::default(),
            memory_loader: None,
            loop_runtime: Some(runtime),
            publication_policy: policy,
            ordinary_parent: task.is_none().then(|| self.clone()),
            operator_task: None,
            operator_process: None,
            ordinary_task: task,
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
            durable_key: false,
            recordable: false,
            registry,
            execution,
            memory: Arc::clone(&parent.memory),
            security: Arc::clone(&parent.security),
            store: Arc::clone(&parent.store),
            durable_events: None,
        }))
    }

    /// Validate frozen scope and live backend bindings for an already issued
    /// task. The task authority separately checks its own cancellation lease.
    pub(crate) fn verify_task_scope(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.0.service.generation.effective.causal_tree.enabled,
            "ordinary task requires disabled plane"
        );
        self.0.scope.validate_runtime(
            &self.0.service.state_key,
            self.0.source.owner_id(),
            self.0.source.workspace_id(),
            *self.0.service.catalog.generation(),
        )?;
        self.0
            .service
            .catalog
            .check_child_parity(&self.0.service.execution, self.scope())?;
        Ok(())
    }

    pub(crate) fn allows_tool(&self, name: &str) -> bool {
        self.0.service.catalog.entry_for_public_name(name).is_some_and(|entry| {
            self.scope()
                .gate_inputs()
                .0
                .allowed
                .contains(&crate::tools::capability::contract::CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                })
        })
    }

    pub(crate) fn scope(&self) -> &Arc<crate::tools::capability::child::VerifiedChildScope> {
        &self.0.scope
    }

    pub(crate) fn generation(&self) -> &Arc<crate::config::ConfigGeneration> {
        &self.0.service.generation
    }

    pub(crate) fn model_source(&self) -> (&str, &str, &crate::providers::credentials::CredentialRef) {
        (
            self.0.source.provider_name(),
            self.0.source.model_name(),
            self.0.source.model_binding().credential_reference(),
        )
    }

    pub(crate) fn provider_catalog(&self) -> &crate::providers::catalog::CatalogView {
        &self.0.service.provider_catalog
    }

    pub(crate) fn loop_runtime(&self) -> anyhow::Result<&PublicationLoopRuntime> {
        self.0
            .service
            .loop_runtime
            .as_ref()
            .context("ordinary child parent loop runtime missing")
    }

    pub(crate) fn security(&self) -> &Arc<crate::security::SecurityPolicy> {
        &self.0.service.security
    }

    pub(crate) fn memory(&self) -> &Arc<dyn crate::memory::Memory> {
        &self.0.service.memory
    }

    pub(crate) fn source_fabric(&self) -> crate::memory::MemoryFabric {
        crate::memory::MemoryFabric::new(Arc::clone(self.memory()), self.0.source.workspace_id())
            .with_event_recording(self.generation().effective.memory.event_recording_config())
    }

    pub(crate) fn memory_scope(&self) -> &crate::memory::principal::MemoryWriteContext {
        self.0.source.memory_scope()
    }
}
