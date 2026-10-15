//! Source-bound selection of exact tool schemas for the shared turn loop.
mod operator_process;

mod operator;
pub(crate) use operator::OperatorChildPublication;
mod ordinary_process;
pub(crate) use ordinary_process::OrdinaryProcessLaunch;

use super::{
    Arc, BTreeMap, BTreeSet, CancellationToken, CapabilityAvailabilityLevel, CapabilityRef, CapabilityTarget,
    CatalogScope, CausalDecisionStore, CausalEventSink, ConfigGeneration, Context, DecisionBackendFactory, DecisionKey,
    DirectiveContext, Exposure, FactoryDeps, GateInputs, Memory, Ordering, Retention, SecurityPolicy, StoreMode, Tool,
    ToolExecutionService, TurnDecisionContext, TurnDecisionService, TurnExecutionDirective, TurnLoopRuntime,
    TurnServiceBuild, TurnServiceBuildError, TurnSourceProof, TurnSurfaceKind, directive_from_selection,
    effective_agent_exclusions, pinned_policy_version, validate_agent_retrieval_provenance,
};
use crate::causal_tree::candidate_generator::CandidateSet;
use crate::causal_tree::gates::{GateOutcome, GatePass, HardGates};
use crate::tools::ToolSpec;

impl TurnDecisionService {
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
        super::validate_agent_turn_origin(&self.generation, self.surface, source)?;
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

pub(crate) struct ConstrainedToolSelection {
    pub(crate) roots: BTreeSet<String>,
    pub(crate) specs: Vec<ToolSpec>,
}

struct DecisionResumeDispatch {
    service: Arc<ToolExecutionService>,
    context: crate::tools::ToolExecutionContext,
    command: crate::tools::ToolExecutionCommand,
}

struct ResumeExecutionGuard<'a>(&'a TurnDecisionContext);
impl Drop for ResumeExecutionGuard<'_> {
    fn drop(&mut self) {
        self.0.terminal_execution_trace();
    }
}

/// Read-only provenance for ordinary disabled children. Retaining the actual
/// service preserves its catalog, PMM and runtime extension generation.
#[derive(Clone)]
pub(crate) struct OrdinaryChildPublication(Arc<OrdinaryChildPublicationInner>);

struct OrdinaryChildPublicationInner {
    context: Arc<TurnDecisionContext>,
    scope: Arc<crate::tools::capability::child::VerifiedChildScope>,
    service: Arc<TurnDecisionService>,
    source: super::super::turn_source::OwnedTurnSourceProof,
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
    pub(crate) fn intercepts_writes(&self) -> bool {
        self.0.source.intercepts_writes()
    }

    fn capture(
        service: &Arc<TurnDecisionService>,
        context: &Arc<TurnDecisionContext>,
        source: &dyn TurnSourceProof,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !service.generation.effective.causal_tree.enabled,
            "ordinary publication requires disabled plane"
        );
        Ok(Self(Arc::new(OrdinaryChildPublicationInner {
            context: Arc::clone(context),
            scope: Arc::new(service.verified_delegation_scope(context)?),
            service: Arc::clone(service),
            source: super::super::turn_source::OwnedTurnSourceProof::capture(source),
        })))
    }

    pub(crate) fn verify_caller(&self, caller: &crate::tools::ToolExecutionContext) -> anyhow::Result<()> {
        let source = &self.0.source;
        let envelope = &caller.envelope;
        super::validate_agent_turn_origin(&self.0.service.generation, self.0.service.surface, source)?;
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
    pub(super) fn verify_live(&self) -> anyhow::Result<()> {
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
        task: Arc<super::super::child_source::ordinary_task::OrdinaryTaskAuthority>,
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
        task: Option<Arc<super::super::child_source::ordinary_task::OrdinaryTaskAuthority>>,
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
                transports: &crate::decision::compatible_http::HttpDecisionTransports,
                config_generation: parent.generation.id.0,
                events: Arc::clone(&parent.events),
            },
        )?
        .with_unproven_budget_gate();
        Ok(Arc::new(TurnDecisionService {
            child_recovery: Default::default(),
            memory_loader: None,
            loop_runtime: Some(runtime),
            publication_policy: policy,
            ordinary_parent: task.is_none().then(|| self.clone()),
            operator_task: None,
            operator_process: None,
            ordinary_task: task,
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
                .contains(&crate::causal_tree::branch::CapabilityRef {
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

    pub(crate) fn loop_runtime(&self) -> anyhow::Result<&TurnLoopRuntime> {
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

impl TurnDecisionService {
    pub(crate) fn bind_agent_execution(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
        execution_context: &crate::tools::ToolExecutionContext,
        specs: &[ToolSpec],
    ) -> anyhow::Result<(Arc<ToolExecutionService>, crate::tools::ToolExecutionContext)> {
        anyhow::ensure!(
            context.resume.is_none(),
            "resumed execution requires the dedicated durable claim adapter"
        );
        self.bind_execution(proof, context, execution_context, specs)
    }

    /// Bind the decision-owned capability set and scope while preserving an
    /// ingress-owned canonical execution service. Chat uses this for its
    /// approval router and preparation events. Every actual call still passes
    /// the issued execution binding, whose parity check covers the complete
    /// live catalog before the backend can run.
    pub(crate) fn bind_agent_execution_with_runtime(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
        execution_context: &crate::tools::ToolExecutionContext,
        specs: &[ToolSpec],
        runtime_execution: Option<Arc<ToolExecutionService>>,
    ) -> anyhow::Result<(Arc<ToolExecutionService>, crate::tools::ToolExecutionContext)> {
        let (published_execution, execution_context) =
            self.bind_agent_execution(proof, context, execution_context, specs)?;
        Ok((runtime_execution.unwrap_or(published_execution), execution_context))
    }

    /// Returns only the original prepared command and exact selected schema;
    /// a resume never asks a model to recreate approved arguments.
    fn bind_resume_execution(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
        execution_context: &crate::tools::ToolExecutionContext,
    ) -> anyhow::Result<DecisionResumeDispatch> {
        let resume = context.resume.as_ref().context("resume admission missing")?;
        let (command, dialect) = resume.command()?;
        let selected = self.agent_execution_specs(proof, context)?;
        anyhow::ensure!(
            selected.specs.len() == 1 && selected.specs.first().is_some_and(|spec| spec.name == command.name),
            "resume selection differs from approved call"
        );
        let (service, mut execution_context) =
            self.bind_execution(proof, context, execution_context, &selected.specs)?;
        execution_context.schema_dialect = dialect;
        execution_context.decision_approval = None;
        execution_context.decision_resume = Some(crate::tools::capability::execution_binding::DecisionResumePort::new(
            Arc::new(super::pending_approval::ExecutionResumeAdapter::new(
                Arc::clone(self),
                Arc::clone(context),
                proof,
                execution_context.envelope.clone(),
            )),
        ));
        Ok(DecisionResumeDispatch {
            service,
            context: execution_context,
            command: command.clone(),
        })
    }

    /// Execute exactly the approved call, without a model round trip. The
    /// owning ingress must durably finalize the returned outcome. Cancellation
    /// or dropping this future seals its trace and prevents further dispatch.
    pub(crate) async fn execute_agent_resume(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
        execution_context: &crate::tools::ToolExecutionContext,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<crate::tools::ToolExecutionOutcome> {
        let dispatch = self.bind_resume_execution(proof, context, execution_context)?;
        let _guard = ResumeExecutionGuard(context);
        Ok(dispatch
            .service
            .execute(dispatch.command, dispatch.context, Some(cancellation.clone()))
            .await)
    }

    fn bind_execution(
        self: &Arc<Self>,
        proof: &dyn TurnSourceProof,
        context: &Arc<TurnDecisionContext>,
        execution_context: &crate::tools::ToolExecutionContext,
        specs: &[ToolSpec],
    ) -> anyhow::Result<(Arc<ToolExecutionService>, crate::tools::ToolExecutionContext)> {
        anyhow::ensure!(
            !context.execution_closed.load(Ordering::Acquire),
            "decision execution is already closed"
        );
        anyhow::ensure!(
            specs.is_empty() || proof.tool_calls_allowed() && self.model_can_call_tools(proof),
            "source configuration forbids tool calls"
        );
        let selected = self.agent_execution_specs(proof, context)?;
        let envelope = &execution_context.envelope;
        anyhow::ensure!(
            envelope.owner_isolated == proof.owner_isolated()
                && envelope.workspace_id == proof.workspace_id()
                && envelope.resolved_owner_id() == proof.owner_id()
                && envelope.session_key == proof.session_key()
                && envelope.run_id.as_deref() == Some(proof.run_id())
                && envelope.task_id.as_deref() == proof.task_id()
                && envelope.channel.as_deref() == Some(proof.policy_channel())
                && envelope.sender.as_deref() == Some(proof.policy_sender())
                && execution_context.chat_type == proof.policy_chat_type()
                && envelope.source_message_event_id.as_deref() == proof.source_event_id()
                && envelope.config_generation_id == Some(proof.config_generation_id())
                && envelope.config_source_revision.as_deref() == Some(proof.config_source_revision()),
            "execution scope differs from the authenticated decision source"
        );
        let mut bindings = BTreeMap::new();
        for spec in specs {
            let discovered = context.discovery_admission(&spec.name);
            let expected = if let Some(selected) = selected.specs.iter().find(|item| item.name == spec.name) {
                selected.clone()
            } else if spec.name == "capability_search" {
                self.agent_capability_search_spec(proof, context)?
                    .context("capability search is not permitted for this turn")?
            } else if let Some(admitted) = &discovered {
                let proposal = self
                    .prepare_discovered_tool(proof, context, &spec.name)?
                    .context("discovery offer missing at execution binding")?;
                let pending = matches!(
                    proposal.admission,
                    GateOutcome::ApprovalOnly { .. } | GateOutcome::ApprovalDeferred
                );
                anyhow::ensure!(
                    proposal.reference == admitted.reference
                        && pending == admitted.requires_approval
                        && (proposal.admission == GateOutcome::Passed || pending),
                    "discovered execution capability failed fresh admission"
                );
                proposal.spec
            } else {
                self.agent_runtime_dependency_spec(proof, context, &spec.name)?
            };
            anyhow::ensure!(
                spec.name == expected.name
                    && spec.description == expected.description
                    && spec.parameters == expected.parameters,
                "execution schema differs from the admitted definition"
            );
            let entry = self
                .catalog
                .entry_for_public_name(&spec.name)
                .context("execution binding has no catalog entry")?;
            let binding = crate::tools::capability::execution_binding::ExecutionBinding::issued(
                context.decision_id(),
                entry,
                Arc::clone(&self.catalog),
                envelope.clone(),
                selected.specs.iter().any(|item| item.name == spec.name) || discovered.is_some(),
            )
            .with_required_approval(discovered.as_ref().is_some_and(|admission| admission.requires_approval));
            let binding = if spec.name == "capability_search" {
                binding.with_discovery_control_scope(proof.child_scope().cloned())
            } else {
                binding.with_child_scope(proof.child_scope().cloned())
            };
            anyhow::ensure!(
                bindings.insert(spec.name.clone(), binding).is_none(),
                "duplicate execution binding"
            );
        }
        self.verify_console_filesystem_context(proof, context)?;
        let mut execution_context = execution_context.clone();
        execution_context.owner_filesystem = context.owner_filesystem.clone();
        execution_context.capability_search = bindings
            .contains_key("capability_search")
            .then(|| self.capability_search_port(proof, context))
            .transpose()?;
        execution_context.ordinary_child_publication = (!self.generation.effective.causal_tree.enabled
            && bindings
                .keys()
                .any(|name| matches!(name.as_str(), "delegate" | "sessions_spawn")))
        .then(|| OrdinaryChildPublication::capture(self, context, proof))
        .transpose()?;
        let delegation_bound = bindings
            .keys()
            .any(|name| matches!(name.as_str(), "delegate" | "sessions_spawn"));
        execution_context.child_grant = if delegation_bound && self.generation.effective.causal_tree.enabled {
            Some(self.issue_child_grant(proof, context)?)
        } else if delegation_bound {
            {
                anyhow::ensure!(
                    execution_context.ordinary_child_publication.is_some(),
                    "ordinary delegation execution lacks its parent publication"
                );
            }
            None
        } else {
            None
        };
        execution_context.decision_resume = None;
        execution_context.execution_bindings = Some(Arc::new(bindings));
        execution_context.decision_approval =
            Some(crate::tools::capability::execution_binding::DecisionApprovalPort::new(
                Arc::new(super::pending_approval::ExecutionApprovalRecorder {
                    service: Arc::clone(self),
                    context: Arc::clone(context),
                }),
            ));
        let trace = context.execution_trace.get_or_init(Default::default);
        trace.bind_exact_call_key(Arc::clone(&self.state_key))?;
        execution_context.execution_trace = Some(trace.clone());
        Ok((Arc::clone(&self.execution), execution_context))
    }

    /// Only the loop's fixed runtime dependencies can extend a selected
    /// surface. They still share source, exclusion and availability gates.
    pub(crate) fn agent_runtime_dependency_spec(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        name: &str,
    ) -> anyhow::Result<ToolSpec> {
        anyhow::ensure!(proof.tool_calls_allowed(), "source configuration forbids tool calls");
        anyhow::ensure!(
            matches!(name, "transcript_history_lookup" | "stay_silent"),
            "unknown runtime dependency"
        );
        self.agent_execution_specs(proof, context)?;
        let entry = self
            .catalog
            .entry_for_public_name(name)
            .context("runtime dependency is absent")?;
        let trusted = context.engine.state.trusted.parts();
        if let Some(scope) = proof.child_scope() {
            anyhow::ensure!(
                scope.gate_inputs().0.allowed.contains(&CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                }),
                "runtime dependency is outside child authority"
            );
        }
        anyhow::ensure!(
            entry.generation == trusted.catalog_generation
                && !self.catalog_view().surface_excludes(entry)
                && !trusted.excludes_entry(self.catalog_view().model_admission_entry(entry))
                && (trusted.channel.is_none() || entry.channel_eligible)
                && crate::tools::execution::decide_tool_execution(
                    &self.security,
                    entry.policy_tool_name(),
                    proof.policy_sender(),
                    proof.policy_channel(),
                    proof.policy_chat_type(),
                ) != crate::tools::execution::ToolExecutionDecision::Deny
                && !matches!(entry.exposure, Exposure::Unavailable { .. })
                && entry.availability.level >= CapabilityAvailabilityLevel::Configured,
            "runtime dependency is excluded, unavailable or stale"
        );
        let descriptor = self
            .execution
            .current_descriptor(name)
            .context("runtime dependency cannot execute")?;
        let spec = ToolSpec {
            name: descriptor.public_name,
            description: descriptor.description,
            parameters: descriptor.parameters,
        };
        anyhow::ensure!(
            crate::tools::capability::fingerprint::tool_spec_fingerprint(
                &spec.name,
                &spec.description,
                &spec.parameters
            ) == entry.fingerprint,
            "runtime dependency schema changed"
        );
        Ok(spec)
    }

    /// Confirm only the actual exposed runtime dependency, before sealing its
    /// decision-owned trace. This does not mint successful tool-call evidence.
    pub(crate) fn record_agent_silence(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
        compiled: &crate::agent::turn_context::CompiledToolContext,
    ) -> anyhow::Result<()> {
        let name = crate::tools::STAY_SILENT_TOOL_NAME;
        anyhow::ensure!(compiled.allows(name), "silence dependency was not exposed");
        let current = self.agent_runtime_dependency_spec(proof, context, name)?;
        let offered = compiled
            .tool_specs()
            .iter()
            .find(|spec| spec.name == name)
            .context("silence dependency schema was not offered")?;
        anyhow::ensure!(
            crate::tools::capability::fingerprint::tool_spec_fingerprint(
                &offered.name,
                &offered.description,
                &offered.parameters
            ) == crate::tools::capability::fingerprint::tool_spec_fingerprint(
                &current.name,
                &current.description,
                &current.parameters
            ),
            "silence dependency changed after provider request"
        );
        context
            .execution_trace
            .get()
            .context("silence has no actual execution trace")?
            .record_authorized_silence()
    }

    /// Recheck the issued decision before each provider iteration. A backend
    /// root with many aliases contributes only the exact selected definitions.
    /// Read only metadata belonging to the final-gated capability set. This
    /// performs no embedding lookup or skill-body load.
    pub(crate) fn agent_prompt_skills(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<Vec<crate::skills::Skill>> {
        if context.approval_collection_proof().is_some() {
            self.validate_approval_collection(proof, context)?;
            return Ok(Vec::new());
        }
        let _ = self.agent_execution_specs(proof, context)?;
        let refs: Vec<_> = context.execution_refs().iter().cloned().collect();
        let entries = self.catalog.resolve(&refs)?;
        let mut skills = Vec::new();
        for entry in entries {
            if entry.family != CapabilityTarget::Skill {
                continue;
            }
            let metadata = entry
                .skill_metadata
                .as_ref()
                .context("selected skill metadata is unavailable")?;
            anyhow::ensure!(
                entry.binding_digest == Some(crate::skills::catalog_skill_binding_digest(metadata)),
                "selected skill metadata changed"
            );
            skills.push(crate::skills::Skill {
                name: metadata.name.clone(),
                description: metadata.description.clone(),
                location: Some(metadata.location.clone()),
                version: String::new(),
                author: None,
                tags: Vec::new(),
                tools: Vec::new(),
                prompts: Vec::new(),
                embedding: None,
            });
        }
        Ok(skills)
    }

    pub(crate) fn validate_approval_collection<'a>(
        &self,
        proof: &dyn TurnSourceProof,
        context: &'a TurnDecisionContext,
    ) -> anyhow::Result<&'a crate::causal_tree::approval_collection::DeferredArgumentProof> {
        anyhow::ensure!(
            context.service_id == self.service_id,
            "decision belongs to another service"
        );
        anyhow::ensure!(
            context.source_actor_matches(Some(proof.policy_channel()), Some(proof.policy_sender())),
            "execution decision belongs to another ingress actor"
        );
        let model = self.resolve_agent_model(proof)?;
        self.check_live_parity()?;
        let catalog = self
            .catalog
            .view(&CatalogScope::default())
            .with_surface(&self.publication_policy);
        let model_ref = format!("{}:{}", model.provider_label(), model.model());
        let exclusions = effective_agent_exclusions(
            &self.generation.effective,
            &self.registry,
            &catalog,
            model.model(),
            &self.policy_version,
            &self.state_key,
        );
        validate_agent_retrieval_provenance(
            proof,
            context.engine.state.trusted.parts(),
            &model_ref,
            &self.policy_version,
            &exclusions.names,
            exclusions.model_admission.as_deref(),
        )?;
        anyhow::ensure!(
            context.engine.state.routing_input.as_str()
                == crate::multimodal::project_image_markers_for_routing(proof.routing_input()).as_ref(),
            "decision input changed"
        );
        anyhow::ensure!(
            context.catalog_generation() == catalog.generation(),
            "decision catalog changed"
        );
        // The decision deadline bounded prediction/rehearsal/hops, not this
        // subsequent main-provider turn. Its owner retains cancellation, idle
        // supervision and cleanup; source/parity/closed checks still apply.
        let collection = context
            .approval_collection_proof()
            .ok_or_else(|| anyhow::anyhow!("collection proof missing"))?;
        anyhow::ensure!(
            matches!(&context.directive, TurnExecutionDirective::CollectApprovalArguments(issued) if issued == collection),
            "collection directive differs from its selected proof"
        );
        anyhow::ensure!(
            proof.approval_review_adapter() == Some(collection.adapter())
                && collection.state_fingerprint() == context.engine.state.material_fingerprint(&self.state_key)
                && collection.capability().generation == catalog.generation()
                && !context.execution_closed.load(std::sync::atomic::Ordering::Acquire)
                && context.execution_refs.is_empty(),
            "collection source or lifetime changed"
        );
        Ok(collection)
    }

    pub(crate) fn agent_execution_specs(
        &self,
        proof: &dyn TurnSourceProof,
        context: &TurnDecisionContext,
    ) -> anyhow::Result<ConstrainedToolSelection> {
        anyhow::ensure!(
            context.service_id == self.service_id,
            "decision belongs to another service"
        );
        anyhow::ensure!(
            context.source_actor_matches(Some(proof.policy_channel()), Some(proof.policy_sender())),
            "execution decision belongs to another ingress actor"
        );
        let model = self.resolve_agent_model(proof)?;
        self.check_live_parity()?;
        let catalog = self
            .catalog
            .view(&CatalogScope::default())
            .with_surface(&self.publication_policy);
        let model_ref = format!("{}:{}", model.provider_label(), model.model());
        let exclusions = effective_agent_exclusions(
            &self.generation.effective,
            &self.registry,
            &catalog,
            model.model(),
            &self.policy_version,
            &self.state_key,
        );
        validate_agent_retrieval_provenance(
            proof,
            context.engine.state.trusted.parts(),
            &model_ref,
            &self.policy_version,
            &exclusions.names,
            exclusions.model_admission.as_deref(),
        )?;
        anyhow::ensure!(
            context.engine.state.routing_input.as_str()
                == crate::multimodal::project_image_markers_for_routing(proof.routing_input()).as_ref(),
            "decision input changed"
        );
        anyhow::ensure!(
            context.catalog_generation() == catalog.generation(),
            "decision catalog changed"
        );
        let selected_id = match &context.engine.selection {
            crate::causal_tree::decision_select::Selection::Commit { candidate_id, .. }
            | crate::causal_tree::decision_select::Selection::Recover { candidate_id, .. } => candidate_id,
            _ => anyhow::bail!("decision has no executable selection"),
        };
        let CandidateSet::Legal { candidates, .. } = &context.engine.candidates else {
            anyhow::bail!("decision has no admitted candidate set");
        };
        let selected = candidates
            .iter()
            .find(|candidate| candidate.candidate_id() == selected_id)
            .context("selected candidate is missing")?;
        // The decision deadline bounds prediction/rehearsal/hops. Once the
        // service has issued a context, later execution rechecks live gates;
        // it must not reuse that short deadline as the tool runtime timeout.
        let resume_evidence = context
            .resume
            .as_ref()
            .map(|resume| resume.gate_evidence(self, proof))
            .transpose()?;
        let call_fingerprints = resume_evidence
            .as_ref()
            .map_or_else(BTreeMap::new, |evidence| evidence.calls.clone());
        let replayed_non_idempotent = BTreeSet::new();
        let gates = GateInputs {
            policy: &self.security,
            owner_id: proof.owner_id(),
            sender: proof.policy_sender(),
            policy_channel: proof.policy_channel(),
            chat_type: proof.policy_chat_type(),
            model_ref: &model_ref,
            model_can_chat: true,
            model_can_call_tools: self.model_can_call_tools(proof),
            decision_endpoint_ready: true,
            child_scope: proof.child_scope().map(|scope| scope.gate_inputs()),
            verified_approval_calls: resume_evidence
                .as_ref()
                .map_or_else(BTreeMap::new, |evidence| evidence.approvals.clone()),
            call_fingerprints: &call_fingerprints,
            failed_call_counts: BTreeMap::new(),
            replayed_non_idempotent: &replayed_non_idempotent,
            retry_limit: None,
        };
        let selected_references: BTreeSet<_> = selected.required_capabilities().iter().cloned().collect();
        let references = match &context.directive {
            TurnExecutionDirective::AnswerDirectly => {
                anyhow::ensure!(
                    selected.action() == crate::causal_tree::branch::CausalActionKind::Answer
                        && selected_references.is_empty(),
                    "answer directive differs from its selected candidate"
                );
                let direct = super::direct_answer_refs(&context.engine, &catalog, &gates, &context.execution_refs);
                anyhow::ensure!(
                    direct == context.execution_refs,
                    "answer direct surface changed after admission"
                );
                direct
            }
            TurnExecutionDirective::Execute { allowed_capability_ids } => {
                let ids: BTreeSet<_> = selected_references.iter().map(|reference| &reference.id).collect();
                anyhow::ensure!(
                    selected.action() == crate::causal_tree::branch::CausalActionKind::UseTool
                        && !selected_references.is_empty()
                        && selected_references.len() == selected.required_capabilities().len()
                        && ids.len() == allowed_capability_ids.len()
                        && allowed_capability_ids.iter().all(|id| ids.contains(id))
                        && selected_references == context.execution_refs,
                    "issued execution directive differs from its selected candidate"
                );
                selected_references
            }
            TurnExecutionDirective::Delegate { manifest } => {
                let checked = directive_from_selection(
                    &context.engine,
                    &DirectiveContext {
                        current_generation: catalog.generation(),
                        approval_expires_at: chrono::Utc::now(),
                        discovery_limit: 1,
                    },
                )?;
                anyhow::ensure!(
                    matches!(checked, TurnExecutionDirective::Delegate { manifest: checked } if checked == *manifest)
                        && !selected_references.is_empty()
                        && selected_references == context.execution_refs
                        && selected_references.len() == selected.required_capabilities().len(),
                    "issued delegation differs from its selected candidate"
                );
                selected_references
            }
            _ => anyhow::bail!("terminal or intermediate directive cannot enter the tool loop"),
        };
        anyhow::ensure!(
            HardGates::check_one(GatePass::PreCommit, &context.engine.state, &catalog, selected, &gates)
                == GateOutcome::Passed,
            "selected capability no longer passes execution gates"
        );
        let references: Vec<_> = references.into_iter().collect();
        let entries = self.catalog.resolve(&references)?;
        let mut roots = BTreeSet::new();
        let mut specs = Vec::with_capacity(entries.len());
        for entry in entries {
            let descriptor = self
                .execution
                .current_descriptor(&entry.public_name)
                .context("selected executable capability is missing")?;
            let spec = ToolSpec {
                name: descriptor.public_name,
                description: descriptor.description,
                parameters: descriptor.parameters,
            };
            anyhow::ensure!(
                crate::tools::capability::fingerprint::tool_spec_fingerprint(
                    &spec.name,
                    &spec.description,
                    &spec.parameters
                ) == entry.fingerprint,
                "selected executable schema changed"
            );
            roots.insert(entry.root.clone());
            specs.push(spec);
        }
        Ok(ConstrainedToolSelection { roots, specs })
    }
}
