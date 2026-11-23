struct ResolvedTool {
    shared_binding_digest: SharedBinding,
    backend: Arc<dyn ToolBackend>,
    descriptor: ToolDescriptor,
}

struct ToolExecutionLease {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    memory: Arc<dyn Memory>,
    reservation: ToolExecutionReservation,
    abandonment_key: String,
    terminal_key: String,
}

/// Immutable descriptor snapshot shared by discovery and execution.
///
/// The catalog is assembled from the exact finalized backend registry received
/// by an entry point. It never infers executable readiness from configuration.
#[derive(Debug, Clone)]
pub struct ToolCatalog {
    descriptors: Arc<[ToolDescriptor]>,
}

impl ToolCatalog {
    fn from_backends(backends: &[Arc<dyn ToolBackend>]) -> Self {
        let descriptors = backends
            .iter()
            .flat_map(|backend| {
                backend.specs().into_iter().map(|spec| {
                    tool_descriptor(
                        backend.root_name(),
                        spec,
                        backend.tier(),
                        backend.categories(),
                        |public_name| backend.adapter_kind(public_name),
                        backend.availability(),
                        backend.process_worker_compatible(),
                    )
                    .with_execution_parameters(backend.execution_parameters_schema())
                })
            })
            .collect();
        Self::from_descriptors(descriptors)
    }

    /// Snapshot a legacy boxed registry for user-facing discovery. This uses
    /// the same descriptor construction as [`ToolExecutionService`].
    #[must_use]
    pub fn from_boxed_registry(registry: &[Box<dyn Tool>]) -> Self {
        Self::from_tools(registry.iter().map(Box::as_ref))
    }

    /// Snapshot any selected set of legacy tools. Entry-point-specific tiering
    /// may choose a subset, but descriptor and availability semantics remain
    /// identical everywhere.
    #[must_use]
    pub fn from_tools<'a>(tools: impl IntoIterator<Item = &'a dyn Tool>) -> Self {
        let descriptors = tools
            .into_iter()
            .flat_map(|tool| {
                tool.specs().into_iter().map(|spec| {
                    let root_name = tool.name();
                    tool_descriptor(
                        root_name,
                        spec,
                        tool.tier(),
                        tool.categories().to_vec(),
                        |public_name| {
                            if root_name == "mcp_call" && public_name != root_name {
                                ToolAdapterKind::McpAlias
                            } else if root_name == "wasm_plugin_call" {
                                ToolAdapterKind::WasmPlugin
                            } else if root_name == "skill_execute" {
                                ToolAdapterKind::Skill
                            } else {
                                ToolAdapterKind::Native
                            }
                        },
                        tool.availability(),
                        tool.process_worker_compatible(),
                    )
                    .with_execution_parameters(tool.execution_parameters_schema())
                })
            })
            .collect();
        Self::from_descriptors(descriptors)
    }

    fn from_descriptors(descriptors: Vec<ToolDescriptor>) -> Self {
        // Preserve registry/spec order so adopting the catalog does not reorder
        // provider prompts. The first executable registration owns a duplicate
        // public name, matching execution resolution's first-match semantics.
        let mut unique = Vec::with_capacity(descriptors.len());
        for descriptor in descriptors {
            if !unique
                .iter()
                .any(|registered: &ToolDescriptor| registered.public_name == descriptor.public_name)
            {
                unique.push(descriptor);
            }
        }
        Self {
            descriptors: unique.into(),
        }
    }

    #[must_use]
    pub fn descriptors(&self) -> &[ToolDescriptor] {
        &self.descriptors
    }

    /// Provider-facing projection of the canonical descriptors.
    #[must_use]
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.descriptors
            .iter()
            .filter(|descriptor| {
                descriptor.availability.level >= crate::capability::CapabilityAvailabilityLevel::Configured
            })
            .map(|descriptor| ToolSpec {
                name: descriptor.public_name.clone(),
                description: descriptor.description.clone(),
                parameters: descriptor.parameters.clone(),
            })
            .collect()
    }

    #[must_use]
    pub fn descriptor(&self, public_name: &str) -> Option<&ToolDescriptor> {
        self.descriptors
            .iter()
            .find(|descriptor| descriptor.public_name == public_name)
    }
}

fn tool_descriptor(
    backend_name: &str,
    spec: ToolSpec,
    tier: ToolTier,
    categories: Vec<ToolCategory>,
    adapter_kind: impl FnOnce(&str) -> ToolAdapterKind,
    availability: CapabilityAvailability,
    process_worker_compatible: bool,
) -> ToolDescriptor {
    let adapter = adapter_kind(&spec.name);
    let effect = if backend_name == "skill_read" || crate::security::policy::is_read_only_tool(&spec.name) {
        ToolEffect::Read
    } else {
        ToolEffect::Act
    };
    let adapter_label = match adapter {
        ToolAdapterKind::Native => "native",
        ToolAdapterKind::McpAlias => "MCP alias",
        ToolAdapterKind::WasmPlugin => "WASM plugin",
        ToolAdapterKind::Skill => "skill",
    };
    let availability = match crate::tools::schema::SchemaCleanr::validate(&spec.parameters) {
        Ok(()) => availability,
        Err(error) => CapabilityAvailability::declared(format!(
            "invalid schema for {adapter_label} tool '{}': {error}",
            spec.name
        )),
    };
    ToolDescriptor {
        public_name: spec.name,
        backend_name: backend_name.to_string(),
        description: spec.description,
        parameters: spec.parameters,
        execution_parameters: None,
        tier,
        categories,
        effect,
        adapter,
        process_worker_compatible,
        availability: if availability.reason.trim().is_empty() {
            CapabilityAvailability::ready(format!(
                "executable {adapter_label} backend '{backend_name}' is registered"
            ))
        } else {
            availability
        },
    }
}

/// Mandatory application pipeline for native tools and MCP aliases.
#[derive(Clone)]
pub struct ToolExecutionService {
    backends: Arc<[Arc<dyn ToolBackend>]>,
    catalog: ToolCatalog,
    policy: Arc<dyn EffectPolicy>,
    approval: Arc<dyn ApprovalStrategy>,
    preparation: Arc<dyn ToolExecutionPreparation>,
    audit: Arc<dyn ToolExecutionAuditSink>,
    idempotency_memory: Option<Arc<dyn Memory>>,
    refusal_notice: Option<Arc<dyn ToolRefusalNotice>>,
    #[cfg(test)]
    post_reservation_gate: Option<Arc<TestPostReservationGate>>,
}

#[cfg(test)]
struct TestPostReservationGate {
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl ToolExecutionService {
    /// The same service under another approval strategy. The backends,
    /// catalog, effect policy, preparation, audit and idempotency ledger are
    /// shared, so only the approval decision differs (a task child's turn runs
    /// its published service under its run's approval strategy).
    #[must_use]
    pub(crate) fn with_approval(&self, approval: Arc<dyn ApprovalStrategy>) -> Self {
        let mut service = self.clone();
        service.approval = approval;
        service
    }

    #[must_use]
    pub fn new(
        tools: Vec<Arc<dyn Tool>>,
        policy: Arc<dyn EffectPolicy>,
        approval: Arc<dyn ApprovalStrategy>,
        preparation: Arc<dyn ToolExecutionPreparation>,
        audit: Arc<dyn ToolExecutionAuditSink>,
    ) -> Self {
        let backends = tools
            .into_iter()
            .map(|tool| Arc::new(LegacyToolAdapter::new(tool)) as Arc<dyn ToolBackend>)
            .collect();
        Self::from_backends(backends, policy, approval, preparation, audit)
    }

    /// Assemble the service from raw execution ports.
    #[must_use]
    pub fn from_backends(
        backends: Vec<Arc<dyn ToolBackend>>,
        policy: Arc<dyn EffectPolicy>,
        approval: Arc<dyn ApprovalStrategy>,
        preparation: Arc<dyn ToolExecutionPreparation>,
        audit: Arc<dyn ToolExecutionAuditSink>,
    ) -> Self {
        let catalog = ToolCatalog::from_backends(&backends);
        Self {
            backends: backends.into(),
            catalog,
            policy,
            approval,
            preparation,
            audit,
            idempotency_memory: None,
            refusal_notice: None,
            #[cfg(test)]
            post_reservation_gate: None,
        }
    }

    /// Consume the current boxed registry without cloning tool state.
    #[must_use]
    pub fn from_boxed(
        tools: Vec<Box<dyn Tool>>,
        policy: Arc<dyn EffectPolicy>,
        approval: Arc<dyn ApprovalStrategy>,
        preparation: Arc<dyn ToolExecutionPreparation>,
        audit: Arc<dyn ToolExecutionAuditSink>,
    ) -> Self {
        let tools = tools.into_iter().map(Arc::<dyn Tool>::from).collect();
        Self::new(tools, policy, approval, preparation, audit)
    }

    /// Adapt a shared boxed registry without cloning or consuming stateful
    /// tools. This is the compatibility assembly used by Chat Redux.
    #[must_use]
    pub fn from_shared_boxed_registry(
        registry: Arc<Vec<Box<dyn Tool>>>,
        policy: Arc<dyn EffectPolicy>,
        approval: Arc<dyn ApprovalStrategy>,
        preparation: Arc<dyn ToolExecutionPreparation>,
        audit: Arc<dyn ToolExecutionAuditSink>,
    ) -> Self {
        let backends = registry
            .iter()
            .map(|tool| tool.name().to_string())
            .map(|root_name| {
                Arc::new(SharedRegistryToolAdapter {
                    registry: Arc::clone(&registry),
                    root_name,
                }) as Arc<dyn ToolBackend>
            })
            .collect();
        Self::from_backends(backends, policy, approval, preparation, audit)
    }

    /// Fail-closed convenience assembly for non-interactive callers.
    #[must_use]
    pub fn with_security_policy(tools: Vec<Box<dyn Tool>>, policy: Arc<SecurityPolicy>) -> Self {
        Self::from_boxed(
            tools,
            Arc::new(SecurityEffectPolicy::new(policy)),
            Arc::new(DenyApprovalStrategy),
            Arc::new(AdapterOwnedPreparation),
            Arc::new(TracingToolExecutionAudit),
        )
    }

    /// Attach the durable MessageEvent ledger used to reserve and replay
    /// idempotent tool executions.
    ///
    /// Callers resolve the ledger through `crate::memory::tool_execution_ledger`,
    /// which already guarantees a ledger-capable backend. The check here stays as
    /// a last line of defence: a backend without a durable event log would
    /// otherwise record reservations that never survive a crash.
    #[must_use]
    pub fn with_idempotency_memory(mut self, memory: Arc<dyn Memory>) -> Self {
        if crate::memory::serves_tool_execution_ledger(memory.name()) {
            self.idempotency_memory = Some(memory);
        } else {
            tracing::warn!(
                backend = memory.name(),
                "tool execution idempotency is unavailable for this memory backend"
            );
        }
        self
    }

    /// Show the operator each call refused before approval (plain stderr
    /// line or TUI notice).
    #[must_use]
    pub(crate) fn with_refusal_notice(mut self, notice: Arc<dyn ToolRefusalNotice>) -> Self {
        self.refusal_notice = Some(notice);
        self
    }

    #[cfg(test)]
    fn with_post_reservation_gate(mut self, gate: Arc<TestPostReservationGate>) -> Self {
        self.post_reservation_gate = Some(gate);
        self
    }

    fn resolve(&self, public_name: &str) -> Option<ResolvedTool> {
        // Always inspect the current exact spec. A startup descriptor can be
        // stale after MCP, WASM or Skill discovery changes its schema, and a
        // live MCP catalog can be larger than the provider-facing spec window.
        let pinned_root = self
            .catalog
            .descriptor(public_name)
            .map(|entry| entry.backend_name.as_str());
        let mut resolved = None;
        for backend in self.backends.iter() {
            if pinned_root.is_some_and(|root| root != backend.root_name()) {
                continue;
            }
            if !backend.supports_name(public_name) {
                continue;
            }
            let (spec, shared_binding_digest) = if let Some(snapshot) = backend.shared_catalog_snapshot() {
                let Some(entry) = snapshot.entry(public_name) else {
                    continue;
                };
                (entry.spec().clone(), SharedBinding::Snapshot(entry.binding_digest()))
            } else {
                let Some(spec) = backend.spec_for_name(public_name) else {
                    continue;
                };
                (spec, SharedBinding::Legacy)
            };
            let descriptor = tool_descriptor(
                backend.root_name(),
                spec,
                backend.tier(),
                backend.categories(),
                |name| backend.adapter_kind(name),
                backend.availability(),
                backend.process_worker_compatible(),
            )
            .with_execution_parameters(backend.execution_parameters_schema());
            if resolved.is_some() {
                // A dynamic name with more than one live owner is ambiguous.
                return None;
            }
            resolved = Some(ResolvedTool {
                shared_binding_digest,
                backend: Arc::clone(backend),
                descriptor,
            });
        }
        resolved
    }

    /// Descriptor for the exact capability the execution service can run now.
    #[must_use]
    pub fn current_descriptor(&self, public_name: &str) -> Option<ToolDescriptor> {
        self.resolve(public_name).map(|resolved| resolved.descriptor)
    }

    /// Snapshot taken when the service was built; schema drift compares this
    /// to the live descriptor.
    pub(crate) fn admitted_descriptor(&self, public_name: &str) -> Option<&ToolDescriptor> {
        self.catalog.descriptor(public_name)
    }

    pub(crate) fn current_descriptor_with_binding(
        &self,
        name: &str,
        require_binding: bool,
    ) -> Option<(ToolDescriptor, Option<[u8; 32]>)> {
        let resolved = self.resolve(name)?;
        let binding = resolved.shared_binding_digest.or_else_live(|| {
            require_binding
                .then(|| resolved.backend.catalog_binding_snapshot().get(name).copied())
                .flatten()
        });
        Some((resolved.descriptor, binding))
    }

    pub(crate) fn current_binding_digest(&self, root: &str, name: &str) -> Option<[u8; 32]> {
        let owner = self.catalog.descriptor(root)?.backend_name.as_str();
        let backend = self.backends.iter().find(|backend| backend.root_name() == owner)?;
        backend.shared_catalog_snapshot().map_or_else(
            || backend.catalog_binding_snapshot().get(name).copied(),
            |snapshot| snapshot.binding_digest(name),
        )
    }

    pub(crate) fn supports_owner_filesystem(&self, public_name: &str) -> bool {
        self.resolve(public_name).is_some_and(|resolved| {
            resolved.descriptor.adapter == ToolAdapterKind::Native
                && resolved.descriptor.backend_name == public_name
                && matches!(public_name, "file_read" | "file_write" | "file_edit")
                && resolved.backend.supports_owner_filesystem()
        })
    }

    /// Full live inventory. A collision with a pinned startup name is
    /// reported by parity even if the service still routes to its first owner.
    #[must_use]
    pub fn current_descriptors(&self) -> Vec<ToolDescriptor> {
        self.backends
            .iter()
            .flat_map(|backend| {
                backend.catalog_specs().into_iter().map(|spec| {
                    tool_descriptor(
                        backend.root_name(),
                        spec,
                        backend.tier(),
                        backend.categories(),
                        |name| backend.adapter_kind(name),
                        backend.availability(),
                        backend.process_worker_compatible(),
                    )
                    .with_execution_parameters(backend.execution_parameters_schema())
                })
            })
            .collect()
    }

    /// Live inventory paired with each entry's dispatch binding, in one pass
    /// over the backends. The binding is read the same way `resolve` reads it,
    /// but each backend's snapshot and binding map are captured only once.
    #[must_use]
    pub(crate) fn live_descriptors_with_bindings(&self) -> Vec<(ToolDescriptor, Option<[u8; 32]>)> {
        let mut out = Vec::new();
        for backend in self.backends.iter() {
            let shared = backend.shared_catalog_snapshot();
            let mut legacy_bindings = None;
            for spec in backend.catalog_specs() {
                let binding = if let Some(snapshot) = &shared {
                    snapshot.entry(&spec.name).and_then(|entry| entry.binding_digest())
                } else {
                    legacy_bindings
                        .get_or_insert_with(|| backend.catalog_binding_snapshot())
                        .get(&spec.name)
                        .copied()
                };
                let descriptor = tool_descriptor(
                    backend.root_name(),
                    spec,
                    backend.tier(),
                    backend.categories(),
                    |name| backend.adapter_kind(name),
                    backend.availability(),
                    backend.process_worker_compatible(),
                )
                .with_execution_parameters(backend.execution_parameters_schema());
                out.push((descriptor, binding));
            }
        }
        out
    }

    /// Full fresh inventory for causal parity without building user-facing
    /// descriptors. A malformed schema has the same Declared status as the
    /// descriptor path and therefore cannot satisfy a pinned configured entry.
    #[must_use]
    pub(crate) fn current_specs_for_parity(&self) -> Vec<CurrentToolSpec<'_>> {
        // Capture each dynamic publication once before allocating the flat view.
        // Startup descriptors may contain only a dynamic backend's root.
        let snapshots: Vec<_> = self
            .backends
            .iter()
            .map(|backend| backend.shared_catalog_snapshot())
            .collect();
        let capacity = self.catalog.descriptors().len()
            + snapshots
                .iter()
                .flatten()
                .map(|snapshot| snapshot.entries().len())
                .sum::<usize>();
        let mut live = Vec::with_capacity(capacity);
        for (backend, snapshot) in self.backends.iter().zip(snapshots) {
            let backend = backend.as_ref();
            if let Some(snapshot) = snapshot {
                let configured = backend.catalog_availability_is_configured();
                for entry in snapshot.entries() {
                    live.push(CurrentToolSpec {
                        backend_name: backend.root_name(),
                        configured: configured && entry.schema_valid(),
                        spec: CurrentToolSpecValue::Shared {
                            entry: Arc::clone(entry),
                            snapshot: snapshot.clone(),
                        },
                    });
                }
            } else if let Some(specs) = backend.immutable_catalog_specs() {
                for spec in specs {
                    live.push(current_tool_spec(backend, std::borrow::Cow::Borrowed(spec)));
                }
            } else {
                for spec in backend.catalog_specs() {
                    live.push(current_tool_spec(backend, std::borrow::Cow::Owned(spec)));
                }
            }
        }
        live
    }

    #[must_use]
    pub fn current_binding_snapshot(&self, root: &str) -> Option<std::collections::BTreeMap<String, [u8; 32]>> {
        let owner = self.catalog.descriptor(root)?.backend_name.as_str();
        Some(
            self.backends
                .iter()
                .find(|backend| backend.root_name() == owner)?
                .catalog_binding_snapshot(),
        )
    }

    /// Return whether an exact public tool name can currently be resolved.
    ///
    /// This includes live dynamic aliases that were discovered after the
    /// immutable startup catalog was captured. Turn-level exposure guards use
    /// this distinction to reject a known-but-unexposed capability without
    /// changing the existing error for a genuinely unknown tool name.
    #[must_use]
    pub fn supports_public_name(&self, public_name: &str) -> bool {
        self.resolve(public_name).is_some()
    }

    /// Return the normal execution-policy denial for a known capability that
    /// was withheld from this turn. This performs no binding, approval,
    /// preparation, reservation, or backend work.
    #[must_use]
    pub fn unexposed_policy_denial(&self, public_name: &str, context: &ToolExecutionContext) -> Option<String> {
        let descriptor = self.resolve(public_name)?.descriptor;
        (self.policy.decide(&descriptor, context) == ToolExecutionDecision::Deny).then(|| {
            format!(
                "tool '{}' is not permitted under the current execution policy",
                descriptor.public_name
            )
        })
    }

    #[must_use]
    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.catalog.descriptors().to_vec()
    }

    #[must_use]
    pub const fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    async fn prepare_idempotent_execution(
        &self,
        command: &ToolExecutionCommand,
        context: &ToolExecutionContext,
        input_sha256: &str,
        descriptor: &ToolDescriptor,
        decision: ToolExecutionDecision,
        preparation: &ToolExecutionPermit,
        started: Instant,
    ) -> Result<Option<ToolExecutionLease>, Box<ToolExecutionOutcome>> {
        let Some(idempotency_key) = command.idempotency_key.as_deref() else {
            return Ok(None);
        };
        let Some(memory) = self.idempotency_memory.as_ref() else {
            if descriptor.effect == ToolEffect::Read {
                return Ok(None);
            }
            return Err(Box::new(self.finish(
                command,
                context,
                input_sha256.to_string(),
                Some(descriptor.clone()),
                Some(decision),
                ToolExecutionStatus::Indeterminate,
                "Error: durable tool idempotency ledger is unavailable; refusing side effect".to_string(),
                None,
                Some("durable tool idempotency ledger is unavailable".to_string()),
                Some(preparation.clone()),
                started,
            )));
        };
        let lock = tool_execution_lock(
            &context.envelope.workspace_id,
            &context.envelope.session_key,
            &context.semantic_turn_id,
            idempotency_key,
        );
        let guard = lock.lock_owned().await;
        let (base_reservation_key, terminal_key) = tool_execution_ledger_keys(context, idempotency_key);

        match memory
            .find_message_event_by_idempotency_key(&context.envelope.workspace_id, &terminal_key)
            .await
        {
            Ok(Some(event)) => {
                let terminal = event
                    .raw_payload_json
                    .as_deref()
                    .and_then(|payload| serde_json::from_str::<ToolExecutionTerminalRecord>(payload).ok());
                let Some(terminal) = terminal else {
                    return Err(Box::new(self.finish(
                        command,
                        context,
                        input_sha256.to_string(),
                        Some(descriptor.clone()),
                        Some(decision),
                        ToolExecutionStatus::Indeterminate,
                        "Error: persisted tool terminal record is unreadable; refusing replay".to_string(),
                        None,
                        Some("persisted tool terminal record is unreadable".to_string()),
                        Some(preparation.clone()),
                        started,
                    )));
                };
                if terminal.reservation.capability != command.name || terminal.reservation.input_sha256 != input_sha256
                {
                    return Err(Box::new(self.finish(
                        command,
                        context,
                        input_sha256.to_string(),
                        Some(descriptor.clone()),
                        Some(decision),
                        ToolExecutionStatus::IdempotencyConflict,
                        "Error: idempotency key was already used for a different tool command".to_string(),
                        None,
                        Some("idempotency key command fingerprint mismatch".to_string()),
                        Some(preparation.clone()),
                        started,
                    )));
                }
                return Err(Box::new(self.replay_outcome(
                    command,
                    context,
                    input_sha256,
                    terminal.outcome,
                )));
            }
            Ok(None) => {}
            Err(error) => {
                return Err(Box::new(self.finish(
                    command,
                    context,
                    input_sha256.to_string(),
                    Some(descriptor.clone()),
                    Some(decision),
                    ToolExecutionStatus::Indeterminate,
                    format!("Error: unable to inspect tool idempotency ledger: {error}"),
                    None,
                    Some(error.to_string()),
                    Some(preparation.clone()),
                    started,
                )));
            }
        }

        let mut attempt = 0_u32;
        loop {
            let reservation_key = tool_execution_reservation_key(&base_reservation_key, attempt);
            match memory
                .find_message_event_by_idempotency_key(&context.envelope.workspace_id, &reservation_key)
                .await
            {
                Ok(Some(event)) => {
                    let existing = event
                        .raw_payload_json
                        .as_deref()
                        .and_then(|payload| serde_json::from_str::<ToolExecutionReservation>(payload).ok());
                    let Some(existing) = existing else {
                        return Err(Box::new(
                            self.finish(
                                command,
                                context,
                                input_sha256.to_string(),
                                Some(descriptor.clone()),
                                Some(decision),
                                ToolExecutionStatus::Indeterminate,
                                "Error: persisted tool reservation is unreadable; refusing duplicate side effect"
                                    .to_string(),
                                None,
                                Some("persisted tool reservation is unreadable".to_string()),
                                Some(preparation.clone()),
                                started,
                            ),
                        ));
                    };
                    if existing.capability != command.name || existing.input_sha256 != input_sha256 {
                        return Err(Box::new(self.finish(
                            command,
                            context,
                            input_sha256.to_string(),
                            Some(descriptor.clone()),
                            Some(decision),
                            ToolExecutionStatus::IdempotencyConflict,
                            "Error: idempotency key was already used for a different tool command".to_string(),
                            None,
                            Some("idempotency key command fingerprint mismatch".to_string()),
                            Some(preparation.clone()),
                            started,
                        )));
                    }
                    let abandonment_key = tool_execution_abandonment_key(&base_reservation_key, attempt);
                    let abandoned = memory
                        .find_message_event_by_idempotency_key(&context.envelope.workspace_id, &abandonment_key)
                        .await
                        .map_err(|error| {
                            self.finish(
                                command,
                                context,
                                input_sha256.to_string(),
                                Some(descriptor.clone()),
                                Some(decision),
                                ToolExecutionStatus::Indeterminate,
                                format!("Error: unable to inspect tool idempotency abandonment: {error}"),
                                None,
                                Some(error.to_string()),
                                Some(preparation.clone()),
                                started,
                            )
                        })?;
                    let Some(abandoned) = abandoned else {
                        return Err(Box::new(self.finish(
                            command,
                            context,
                            input_sha256.to_string(),
                            Some(descriptor.clone()),
                            Some(decision),
                            ToolExecutionStatus::Indeterminate,
                            "Error: tool execution was reserved without a terminal outcome; refusing duplicate side effect"
                                .to_string(),
                            None,
                            Some(
                                "tool execution was reserved without a terminal outcome; refusing duplicate side effect"
                                    .to_string(),
                            ),
                            Some(preparation.clone()),
                            started,
                        )));
                    };
                    let abandoned = abandoned
                        .raw_payload_json
                        .as_deref()
                        .and_then(|payload| serde_json::from_str::<ToolExecutionReservation>(payload).ok());
                    if abandoned.as_ref().is_none_or(|abandoned| {
                        abandoned.reservation_id != existing.reservation_id
                            || abandoned.attempt != existing.attempt
                            || abandoned.capability != existing.capability
                            || abandoned.input_sha256 != existing.input_sha256
                    }) {
                        return Err(Box::new(
                            self.finish(
                                command,
                                context,
                                input_sha256.to_string(),
                                Some(descriptor.clone()),
                                Some(decision),
                                ToolExecutionStatus::Indeterminate,
                                "Error: persisted tool abandonment is unreadable or does not own the reservation"
                                    .to_string(),
                                None,
                                Some("persisted tool abandonment does not match reservation owner".to_string()),
                                Some(preparation.clone()),
                                started,
                            ),
                        ));
                    }
                    attempt = attempt.checked_add(1).ok_or_else(|| {
                        self.finish(
                            command,
                            context,
                            input_sha256.to_string(),
                            Some(descriptor.clone()),
                            Some(decision),
                            ToolExecutionStatus::Indeterminate,
                            "Error: tool idempotency retry generation overflow".to_string(),
                            None,
                            Some("tool idempotency retry generation overflow".to_string()),
                            Some(preparation.clone()),
                            started,
                        )
                    })?;
                }
                Ok(None) => {
                    let reservation = ToolExecutionReservation {
                        operation_id: command.operation_id.clone(),
                        reservation_id: Uuid::now_v7().to_string(),
                        attempt,
                        capability: command.name.clone(),
                        input_sha256: input_sha256.to_string(),
                    };
                    let payload = serde_json::to_string(&reservation).map_err(|error| {
                        self.finish(
                            command,
                            context,
                            input_sha256.to_string(),
                            Some(descriptor.clone()),
                            Some(decision),
                            ToolExecutionStatus::Indeterminate,
                            format!("Error: unable to encode tool idempotency reservation: {error}"),
                            None,
                            Some(error.to_string()),
                            Some(preparation.clone()),
                            started,
                        )
                    })?;
                    let event = memory
                        .append_message_event(tool_execution_event_input(
                            context,
                            &reservation_key,
                            "tool.execution.reserved",
                            format!(
                                "operation_id={} capability={} attempt={attempt}",
                                command.operation_id, command.name
                            ),
                            payload,
                        ))
                        .await
                        .map_err(|error| {
                            self.finish(
                                command,
                                context,
                                input_sha256.to_string(),
                                Some(descriptor.clone()),
                                Some(decision),
                                ToolExecutionStatus::Indeterminate,
                                format!("Error: unable to reserve idempotent tool execution: {error}"),
                                None,
                                Some(error.to_string()),
                                Some(preparation.clone()),
                                started,
                            )
                        })?;
                    // The reservation is a durable write the turn owed, and it
                    // sits between `ToolStart` and the backend's first poll
                    // with nothing else to show for it. Recorded now that it
                    // has come back, so a turn whose ledger is merely slow is
                    // not read as a turn that went silent — and so the hang
                    // report, when one is warranted, names where the turn
                    // actually was.
                    crate::agent::idle::beat(crate::agent::idle::ProgressKind::DurableWrite);
                    let recorded = event
                        .raw_payload_json
                        .as_deref()
                        .and_then(|payload| serde_json::from_str::<ToolExecutionReservation>(payload).ok());
                    if recorded
                        .as_ref()
                        .is_none_or(|recorded| recorded.reservation_id != reservation.reservation_id)
                    {
                        return Err(Box::new(self.finish(
                            command,
                            context,
                            input_sha256.to_string(),
                            Some(descriptor.clone()),
                            Some(decision),
                            ToolExecutionStatus::Indeterminate,
                            "Error: another runtime owns this idempotent tool execution".to_string(),
                            None,
                            Some("another runtime owns this idempotent tool execution".to_string()),
                            Some(preparation.clone()),
                            started,
                        )));
                    }
                    return Ok(Some(ToolExecutionLease {
                        _guard: guard,
                        memory: Arc::clone(memory),
                        abandonment_key: tool_execution_abandonment_key(&base_reservation_key, attempt),
                        reservation,
                        terminal_key,
                    }));
                }
                Err(error) => {
                    return Err(Box::new(self.finish(
                        command,
                        context,
                        input_sha256.to_string(),
                        Some(descriptor.clone()),
                        Some(decision),
                        ToolExecutionStatus::Indeterminate,
                        format!("Error: unable to inspect tool idempotency reservation: {error}"),
                        None,
                        Some(error.to_string()),
                        Some(preparation.clone()),
                        started,
                    )));
                }
            }
        }
    }

    async fn commit_idempotent_execution(
        &self,
        lease: Option<&ToolExecutionLease>,
        context: &ToolExecutionContext,
        outcome: &ToolExecutionOutcome,
    ) {
        let Some(lease) = lease else {
            return;
        };
        let terminal = ToolExecutionTerminalRecord {
            reservation: lease.reservation.clone(),
            outcome: outcome.clone(),
        };
        let payload = match serde_json::to_value(&terminal)
            .map(crate::agent::terminal::redact_persisted_json)
            .and_then(|value| serde_json::to_string(&value))
        {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(error = %error, "failed to encode idempotent tool terminal outcome");
                return;
            }
        };
        let appended = lease
            .memory
            .append_message_event(tool_execution_event_input(
                context,
                &lease.terminal_key,
                "tool.execution.finalized",
                format!("operation_id={} status={:?}", outcome.operation_id, outcome.status),
                payload,
            ))
            .await;
        // Same reasoning as the reservation write above, at the other end of
        // the call: this is the last thing the turn does before the loop can
        // announce the result, and until now it was silent.
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::DurableWrite);
        if let Err(error) = appended {
            tracing::error!(
                operation_id = %outcome.operation_id,
                error = %error,
                "tool side effect completed but terminal idempotency outcome was not persisted"
            );
        }
    }

    async fn abandon_idempotent_execution(
        &self,
        lease: Option<&ToolExecutionLease>,
        context: &ToolExecutionContext,
    ) -> Result<(), String> {
        let Some(lease) = lease else {
            return Ok(());
        };
        let payload = serde_json::to_string(&lease.reservation)
            .map_err(|error| format!("unable to encode tool idempotency abandonment: {error}"))?;
        let event = lease
            .memory
            .append_message_event(tool_execution_event_input(
                context,
                &lease.abandonment_key,
                "tool.execution.abandoned",
                format!(
                    "operation_id={} attempt={}",
                    lease.reservation.operation_id, lease.reservation.attempt
                ),
                payload,
            ))
            .await
            .map_err(|error| format!("unable to abandon tool idempotency reservation: {error}"))?;
        let recorded = event
            .raw_payload_json
            .as_deref()
            .and_then(|payload| serde_json::from_str::<ToolExecutionReservation>(payload).ok());
        if recorded.as_ref().is_none_or(|recorded| {
            recorded.reservation_id != lease.reservation.reservation_id
                || recorded.attempt != lease.reservation.attempt
                || recorded.capability != lease.reservation.capability
                || recorded.input_sha256 != lease.reservation.input_sha256
        }) {
            return Err("another runtime owns the tool idempotency abandonment".to_string());
        }
        Ok(())
    }

    fn replay_outcome(
        &self,
        command: &ToolExecutionCommand,
        context: &ToolExecutionContext,
        input_sha256: &str,
        mut outcome: ToolExecutionOutcome,
    ) -> ToolExecutionOutcome {
        outcome.operation_id = command.operation_id.clone();
        outcome.replayed = true;
        self.audit.record(ToolExecutionAuditRecord {
            operation_id: command.operation_id.clone(),
            capability: command.name.clone(),
            backend_name: outcome.descriptor.as_ref().map(|value| value.backend_name.clone()),
            adapter: outcome.descriptor.as_ref().map(|value| value.adapter),
            effect: outcome.descriptor.as_ref().map(|value| value.effect),
            decision: outcome.decision,
            status: outcome.status,
            preparation_strategy: outcome.preparation.as_ref().map(|value| value.strategy.clone()),
            input_sha256: input_sha256.to_string(),
            source: context.envelope.source.as_str().to_string(),
            workspace_id: context.envelope.workspace_id.clone(),
            session_key: context.envelope.session_key.clone(),
            run_id: context.envelope.run_id.clone(),
            parent_run_id: context.envelope.parent_run_id.clone(),
            task_id: context.envelope.task_id.clone(),
            error: outcome.error.clone(),
            duration_ms: 0,
        });
        outcome
    }

}
