//! Durable child-input receipts, independent of parent message provenance.
pub(crate) mod completion;
mod parent_authority;
pub(crate) mod recursive_allowance;
pub(crate) use parent_authority::ParentAuthority;
pub(crate) mod operator_process;
pub(crate) mod operator_process_input;
pub(crate) mod ordinary_process;
pub(crate) mod ordinary_task;
pub(crate) mod parent;
pub(crate) mod publication;
#[cfg(test)]
mod tests;
pub(crate) mod turn_scope;

use crate::memory::{MemoryFabric, MemoryVisibility, MessageEvent, MessageEventScope, MessageEventSource};
use anyhow::Context;
use std::sync::Arc;

pub(crate) struct ChildInputReceipt {
    previous_segment_input: Option<String>,
    segment_issued: bool,
    memory: Arc<dyn crate::memory::Memory>,
    input: Arc<str>,
    event: MessageEvent,
    // Process-local authority cannot be reconstructed from durable event fields.
    authority_guard: Option<crate::memory::RuntimeAuthorityGuard>,
}

/// A process-local sequence marker from an actual source receipt. Its digest
/// compares identity fields only; the private issuer, not the digest, seals it.
pub(in crate::runtime) struct SegmentPredecessor {
    memory: Arc<dyn crate::memory::Memory>,
    event_id: String,
    sequence: i64,
    scope: [u8; 32],
}
impl SegmentPredecessor {
    pub(in crate::runtime) fn event_id(&self) -> &str {
        &self.event_id
    }
}

fn segment_scope_digest(event: &MessageEvent) -> anyhow::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    let Some(crate::memory::MessageEventSubject::Task(task)) = &event.subject else {
        anyhow::bail!("segment receipt has no task identity");
    };
    anyhow::ensure!(
        event.id > 0
            && !event.event_id.is_empty()
            && event.event_id.len() <= crate::memory::ingress_recovery::MAX_ID_BYTES,
        "segment receipt is not durable or exceeds identity bound"
    );
    let mut digest = Sha256::new();
    digest.update(b"prx-private-segment-scope-v1");
    for value in [
        Some(event.workspace_id.as_str()),
        event.owner_id.as_deref(),
        event.session_key.as_deref(),
        Some(task.as_str()),
        Some(event.source.as_str()),
        event.parent_run_id.as_deref(),
        event.parent_session_key.as_deref(),
        event.config_source_revision.as_deref(),
        Some(event.visibility.as_str()),
        event.agent_id.as_deref(),
        event.persona_id.as_deref(),
        event.channel.as_deref(),
        event.sender.as_deref(),
        event.recipient.as_deref(),
        event.correlation_id.as_deref(),
        event.goal_id.as_deref(),
        event.attempt_id.as_deref(),
    ] {
        match value {
            Some(value) => {
                anyhow::ensure!(
                    value.len() <= crate::memory::ingress_recovery::MAX_ID_BYTES,
                    "segment identity exceeds bound"
                );
                digest.update([1]);
                digest.update((value.len() as u64).to_be_bytes());
                digest.update(value.as_bytes());
            }
            None => digest.update([0]),
        }
    }
    digest.update(
        event
            .config_generation_id
            .context("segment generation missing")?
            .to_be_bytes(),
    );
    match event.lease_epoch {
        Some(epoch) => {
            digest.update([1]);
            digest.update(epoch.to_be_bytes());
        }
        None => digest.update([0]),
    }
    Ok(digest.finalize().into())
}

impl std::fmt::Debug for ChildInputReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildInputReceipt").finish_non_exhaustive()
    }
}

impl ChildInputReceipt {
    pub(super) fn previous_segment_input(&self) -> Option<&str> {
        self.previous_segment_input.as_deref()
    }

    /// Advance a private sequence from actual receipts without retaining the
    /// previous user's input or treating a caller-supplied ID as authority.
    pub(in crate::runtime) fn advance_segment(
        current: &mut Arc<Self>,
        previous: Option<&SegmentPredecessor>,
    ) -> anyhow::Result<SegmentPredecessor> {
        anyhow::ensure!(
            !current.segment_issued,
            "segment receipt already issued a sequence marker"
        );
        let event = current.event();
        let identity = segment_scope_digest(event)?;
        if let Some(previous) = previous {
            anyhow::ensure!(
                Arc::ptr_eq(&previous.memory, &current.memory)
                    && event.id > previous.sequence
                    && event.event_id != previous.event_id
                    && identity == previous.scope,
                "segment predecessor belongs to another source sequence"
            );
        }
        let next = SegmentPredecessor {
            memory: Arc::clone(&current.memory),
            event_id: event.event_id.clone(),
            sequence: event.id,
            scope: identity,
        };
        let receipt = Arc::get_mut(current).context("segment input receipt already shared")?;
        anyhow::ensure!(
            receipt.previous_segment_input.is_none(),
            "segment predecessor already bound"
        );
        receipt.previous_segment_input = previous.map(|previous| previous.event_id.clone());
        receipt.segment_issued = true;
        Ok(next)
    }
    pub(crate) fn input(&self) -> &str {
        &self.input
    }
    pub(crate) const fn event(&self) -> &MessageEvent {
        &self.event
    }

    pub(crate) fn verify_backend(&self, memory: &Arc<dyn crate::memory::Memory>) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&self.memory, memory),
            "child receipt belongs to another memory backend"
        );
        Ok(())
    }

    pub(crate) fn execution_context(
        &self,
        proof: &super::turn_source::OwnedTurnSourceProof,
    ) -> anyhow::Result<crate::tools::ToolExecutionContext> {
        use super::turn_source::TurnSourceProof;
        let event = self.event();
        anyhow::ensure!(
            proof.child_scope().is_some()
                && proof.source_event_id() == Some(event.event_id.as_str())
                && proof.workspace_id() == event.workspace_id
                && Some(proof.owner_id()) == event.owner_id.as_deref()
                && Some(proof.session_key()) == event.session_key.as_deref()
                && Some(proof.run_id()) == event.run_id.as_deref()
                && proof.routing_input() == self.input(),
            "child execution context differs from its source proof"
        );
        let mut envelope = match event.source {
            MessageEventSource::SessionsSpawn => super::envelope::RuntimeEnvelope::sessions_spawn(
                proof.workspace_id(),
                proof.session_key(),
                proof.run_id(),
            ),
            MessageEventSource::SessionWorker => super::envelope::RuntimeEnvelope::session_worker(
                proof.workspace_id(),
                proof.session_key(),
                proof.run_id(),
            ),
            MessageEventSource::Delegate => {
                super::envelope::RuntimeEnvelope::delegate(proof.workspace_id(), proof.session_key(), proof.run_id())
            }
            _ => anyhow::bail!("unsupported child execution source"),
        };
        envelope.owner_id = Some(proof.owner_id().to_owned());
        // Preserve the sealed execution boundary independently of memory-write
        // scoping. A child run/session cannot remove owner isolation.
        envelope.owner_isolated = proof.owner_isolated();
        envelope.task_id = proof.task_id().map(str::to_owned);
        envelope.topic_id = match &event.subject {
            Some(crate::memory::MessageEventSubject::Topic(topic)) => Some(topic.clone()),
            _ => None,
        };
        envelope.source_message_event_id = Some(event.event_id.clone());
        envelope.parent_run_id = event.parent_run_id.clone();
        envelope.agent_id = event.agent_id.clone();
        envelope.persona_id = event.persona_id.clone();
        envelope.channel = Some(proof.policy_channel().to_owned());
        envelope.sender = Some(proof.policy_sender().to_owned());
        envelope.recipient = event.recipient.clone();
        envelope.attempt_id = event.attempt_id.clone();
        envelope.lease_epoch = event.lease_epoch;
        envelope.authority_guard = self.authority_guard.clone();
        envelope.config_generation_id = Some(proof.config_generation_id());
        envelope.config_source_revision = Some(proof.config_source_revision().to_owned());
        crate::tools::ToolExecutionContext::new(envelope, proof.policy_chat_type())
            .with_dispatch_origin(crate::tools::DispatchOrigin::ModelTurn)
            .with_incoming_child(proof)
    }
}

/// Synthetic events from disabled recording cannot issue source authority.
/// The caller must require Some before entering enabled child decisions.
pub(crate) async fn record_child_input(
    fabric: &MemoryFabric,
    scope: MessageEventScope,
    input: &str,
    idempotency_key: Option<String>,
    payload: Option<String>,
) -> anyhow::Result<Option<Arc<ChildInputReceipt>>> {
    anyhow::ensure!(
        matches!(
            scope.source,
            MessageEventSource::SessionWorker | MessageEventSource::SessionsSpawn | MessageEventSource::Delegate
        ) && scope.visibility == MemoryVisibility::Workspace,
        "unsupported child input source"
    );
    let event = fabric
        .record_inbound_user_message(scope.clone(), input, idempotency_key, payload)
        .await?;
    if event.id == 0 {
        return Ok(None);
    }
    anyhow::ensure!(
        event.id > 0
            && !event.event_id.is_empty()
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id == scope.owner_id
            && event.source == scope.source
            && event.channel == scope.channel
            && event.session_key == scope.session_key
            && event.session_key.as_deref().is_some_and(|key| !key.is_empty())
            && event.run_id == scope.run_id
            && event.run_id.as_deref().is_some_and(|id| !id.is_empty())
            && event.parent_run_id == scope.parent_run_id
            && event.parent_session_key == scope.parent_session_key
            && event.agent_id == scope.agent_id
            && event.persona_id == scope.persona_id
            && event.sender == scope.sender
            && event.recipient == scope.recipient
            && event.subject == scope.subject
            && event.goal_id == scope.goal_id
            && event.causation_event_id == scope.causation_event_id
            && event.correlation_id == scope.correlation_id
            && event.attempt_id == scope.attempt_id
            && event.lease_epoch == scope.lease_epoch
            && event.config_generation_id == scope.config_generation_id
            && event.config_source_revision == scope.config_source_revision
            && event.visibility == scope.visibility
            && event.role == "user"
            && event.event_type == "message.created"
            && event.content == input,
        "child input receipt differs from requested source"
    );
    let owner = event
        .owner_id
        .as_deref()
        .filter(|owner| !owner.is_empty())
        .context("child input owner is missing")?;
    let durable = fabric
        .memory()
        .find_owned_message_event(fabric.workspace_id(), owner, &event.event_id)
        .await?
        .context("child input receipt is not durable")?;
    anyhow::ensure!(
        serde_json::to_value(&durable)? == serde_json::to_value(&event)?,
        "child input receipt differs from durable record"
    );
    let receipt = Arc::new(ChildInputReceipt {
        previous_segment_input: None,
        segment_issued: false,
        memory: fabric.memory(),
        input: Arc::from(input),
        event,
        authority_guard: scope.authority_guard,
    });
    Ok(Some(receipt))
}

/// Reliability settings for a child whose provider was pinned on purpose
/// (an override, a model hint or a delegated selection). Every path that
/// builds such a child uses this one policy.
///
/// Retry budget, backoff and per-model fallbacks carry over unchanged; the two
/// chain-widening knobs are dropped:
///
/// * `fallback_providers` -- the caller named the provider, so serving the
///   request from another vendor would defeat it while looking like success.
/// * `api_keys` -- cleared defensively so no root rotation credential is
///   visible to the child's settings. The provider factory takes the rotation
///   keys from the resolved credential (`create_provider_for`) and overwrites
///   this field either way, so this does not change what the factory builds.
pub(crate) fn pinned_provider_reliability(base: &crate::config::ReliabilityConfig) -> crate::config::ReliabilityConfig {
    crate::config::ReliabilityConfig {
        fallback_providers: Vec::new(),
        api_keys: Vec::new(),
        ..base.clone()
    }
}

#[cfg(test)]
struct OrdinaryProviderFixture {
    provider: Arc<dyn crate::providers::Provider>,
    provider_name: String,
    model_name: String,
}

#[cfg(test)]
tokio::task_local! {
    static ORDINARY_PROVIDER_FIXTURE: OrdinaryProviderFixture;
}

/// Keep ordinary-child tests on the production admission and binding path
/// while supplying their in-memory provider implementation. Production never
/// enters this scope and always constructs the provider from the pinned model
/// selection below.
#[cfg(test)]
pub(crate) async fn scope_ordinary_provider_fixture<F: std::future::Future>(
    provider: Arc<dyn crate::providers::Provider>,
    provider_name: String,
    model_name: String,
    work: F,
) -> F::Output {
    ORDINARY_PROVIDER_FIXTURE
        .scope(
            OrdinaryProviderFixture {
                provider,
                provider_name,
                model_name,
            },
            work,
        )
        .await
}

pub(super) struct OrdinaryChildModelSelection<'a> {
    pub(super) generation: &'a crate::config::ConfigGeneration,
    pub(super) provider_name: &'a str,
    pub(super) model_name: &'a str,
    pub(super) resolved: &'a crate::providers::catalog::ResolvedModel,
    pub(super) route: Option<crate::providers::catalog::RouteTarget>,
}

/// Owns actual factory output and its source selection. Debug is opaque and
/// serialization is unavailable because the resolved model retains credentials.
pub(crate) struct ChildProviderBinding {
    generation: Arc<crate::config::ConfigGeneration>,
    provider: Arc<dyn crate::providers::Provider>,
    requested_provider: String,
    requested_model: String,
    resolved: crate::providers::catalog::ResolvedModel,
    route: Option<crate::providers::catalog::RouteTarget>,
}

impl std::fmt::Debug for ChildProviderBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChildProviderBinding")
    }
}

impl ChildProviderBinding {
    /// Preserve the provider instance authenticated by the explicit operator
    /// ingress. This model binding alone does not grant child capabilities.
    pub(crate) fn build_operator(source: &super::operator_spawn::OperatorSpawnSource) -> anyhow::Result<Self> {
        use super::turn_source::TurnSourceProof;
        source.verify_live()?;
        let binding = Self {
            generation: Arc::clone(source.generation()),
            provider: Arc::clone(source.provider()),
            requested_provider: source.proof().provider_name().to_owned(),
            requested_model: source.proof().model_name().to_owned(),
            resolved: source.resolved().clone(),
            route: source.route().cloned(),
        };
        binding.verify_operator_source(source)?;
        source.verify_live()?;
        Ok(binding)
    }

    /// Verify retained model provenance independently of the command lifetime.
    /// The task driver separately owns and checks its cancellation authority.
    pub(crate) fn verify_operator_source(
        &self,
        source: &super::operator_spawn::OperatorSpawnSource,
    ) -> anyhow::Result<()> {
        use super::turn_source::TurnSourceProof;
        anyhow::ensure!(
            Arc::ptr_eq(&self.generation, source.generation())
                && Arc::ptr_eq(&self.provider, source.provider())
                && self.requested_provider == source.proof().provider_name()
                && self.requested_model == source.proof().model_name()
                && source.proof().model_binding().matches(&self.resolved)
                && self.route.as_ref() == source.route(),
            "operator child model binding differs from pinned source"
        );
        Ok(())
    }

    /// A disabled ordinary child has model provenance, but no parent authority.
    pub(crate) fn build_ordinary(
        config: &crate::config::Config,
        catalog: &crate::providers::catalog::CatalogView,
        provider_name: &str,
        model_name: &str,
        credential: crate::providers::credentials::CredentialRef,
        publication: Option<&crate::runtime::child_source::publication::OrdinaryChildPublication>,
    ) -> anyhow::Result<Self> {
        let generation = match publication {
            Some(publication) => Arc::clone(publication.generation()),
            None => crate::config::ConfigGeneration::for_child(config)?,
        };
        let catalog = publication.map_or(catalog, |publication| publication.provider_catalog());
        let route = crate::providers::selection::route_target(catalog, provider_name, model_name)?.cloned();
        let resolved = crate::providers::selection::check_with(
            &generation.effective,
            catalog,
            provider_name,
            model_name,
            credential,
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        #[cfg(test)]
        if let Ok(Some(provider)) = ORDINARY_PROVIDER_FIXTURE.try_with(|fixture| {
            ((crate::providers::ProviderId::of(&fixture.provider_name)
                == crate::providers::ProviderId::of(provider_name)
                || crate::providers::canonical_provider_key(&fixture.provider_name)
                    == crate::providers::canonical_provider_key(provider_name))
                && fixture.model_name == model_name)
                .then(|| Arc::clone(&fixture.provider))
        }) {
            return Ok(Self {
                generation,
                provider,
                requested_provider: provider_name.to_owned(),
                requested_model: model_name.to_owned(),
                resolved,
                route,
            });
        }
        Self::from_selection(generation, catalog, provider_name, model_name, resolved, route)
    }

    fn from_selection(
        generation: Arc<crate::config::ConfigGeneration>,
        catalog: &crate::providers::catalog::CatalogView,
        provider_name: &str,
        model_name: &str,
        resolved: crate::providers::catalog::ResolvedModel,
        route: Option<crate::providers::catalog::RouteTarget>,
    ) -> anyhow::Result<Self> {
        let config = &generation.effective;
        // A child provider remains pinned to its selected vendor, matching
        // the existing child contract. The factory validates model fallbacks.
        let reliability = pinned_provider_reliability(&config.reliability);
        let options = crate::providers::provider_runtime_options_from_config(config);
        let provider = Arc::from(crate::providers::create_provider_for(
            &resolved,
            catalog,
            &reliability,
            &config.cost,
            &options,
        )?);
        Ok(Self {
            generation,
            provider,
            requested_provider: provider_name.to_owned(),
            requested_model: model_name.to_owned(),
            resolved,
            route,
        })
    }

    pub(crate) fn provider(&self) -> Arc<dyn crate::providers::Provider> {
        Arc::clone(&self.provider)
    }

    pub(crate) fn model(&self) -> &str {
        self.resolved.model()
    }
    pub(crate) fn provider_name(&self) -> &str {
        self.resolved.provider_label()
    }

    /// Bind ordinary input metadata to this factory instance and local snapshot.
    /// This does not mint a parent grant or a delegated source proof.
    pub(crate) fn bind_ordinary_input_scope(
        &self,
        provider: &Arc<dyn crate::providers::Provider>,
        workspace_id: &str,
        scope: &mut MessageEventScope,
        caller: Option<&crate::tools::execution::ChildCallContext>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(provider, &self.provider)
                && workspace_id == self.generation.effective.workspace_dir.to_string_lossy(),
            "ordinary child input differs from its provider snapshot"
        );
        anyhow::ensure!(
            matches!(
                scope.source,
                MessageEventSource::Delegate | MessageEventSource::SessionsSpawn
            ) && scope.visibility == MemoryVisibility::Workspace
                && scope.run_id.as_deref().is_some_and(|id| !id.is_empty())
                && scope.session_key.as_deref().is_some_and(|id| !id.is_empty())
                && scope.channel.as_deref().is_some_and(|channel| !channel.is_empty()),
            "ordinary delegate input identity is missing"
        );
        if scope.source == MessageEventSource::Delegate {
            anyhow::ensure!(scope.agent_id.is_some(), "ordinary delegate agent missing");
        }
        if let Some(agent) = scope.agent_id.as_deref() {
            let configured = self
                .generation
                .effective
                .agents
                .get(agent)
                .context("ordinary child agent differs from its pinned configuration")?;
            anyhow::ensure!(
                scope.source != MessageEventSource::SessionsSpawn || configured.spawn_enabled.unwrap_or(true),
                "ordinary child agent disallows spawning"
            );
        }
        if let Some(caller) = caller {
            caller.verify_tool(match scope.source {
                MessageEventSource::Delegate => "delegate",
                MessageEventSource::SessionsSpawn => "sessions_spawn",
                _ => anyhow::bail!("unsupported ordinary child source"),
            })?;
            match &caller.context().ordinary_child_publication {
                Some(publication) => {
                    publication.verify_caller(caller.context())?;
                    anyhow::ensure!(
                        Arc::ptr_eq(&self.generation, publication.generation()),
                        "ordinary child config differs from its parent publication"
                    );
                }
                None => anyhow::ensure!(
                    caller.context().admitted_binding.is_none(),
                    "ordinary model child has no issued publication snapshot"
                ),
            }
            let envelope = &caller.context().envelope;
            anyhow::ensure!(
                envelope.workspace_id == workspace_id
                    && !envelope.session_key.trim().is_empty()
                    && envelope.run_id.as_deref() != scope.run_id.as_deref()
                    && envelope
                        .channel
                        .as_deref()
                        .is_some_and(|value| !value.trim().is_empty())
                    && envelope.sender.as_deref().is_some_and(|value| !value.trim().is_empty())
                    && scope.channel == envelope.channel
                    && scope.sender == envelope.sender
                    && (!envelope.owner_isolated
                        || envelope
                            .owner_id
                            .as_deref()
                            .is_some_and(|owner| !owner.trim().is_empty())),
                "ordinary delegate input differs from its issued caller"
            );
            scope.owner_id = Some(envelope.resolved_owner_id());
            scope.parent_run_id = envelope.run_id.clone();
            scope.parent_session_key = Some(envelope.session_key.clone());
            scope.causation_event_id = envelope.source_message_event_id.clone();
            scope.attempt_id = envelope.attempt_id.clone();
            scope.lease_epoch = envelope.lease_epoch;
            scope.authority_guard = envelope.authority_guard.clone();
            scope.persona_id = envelope.persona_id.clone();
        }
        scope.config_generation_id = Some(self.generation.id.0);
        scope.config_source_revision = Some(self.generation.source_revision.fingerprint_sha256.clone());
        Ok(())
    }

    pub(crate) fn ordinary_source_proof(
        &self,
        provider: &Arc<dyn crate::providers::Provider>,
        caller: &crate::tools::execution::ChildCallContext,
        scope: &MessageEventScope,
        input: &str,
        receipt: Option<&ChildInputReceipt>,
    ) -> anyhow::Result<super::turn_source::OwnedTurnSourceProof> {
        let mut bound = scope.clone();
        self.bind_ordinary_input_scope(
            provider,
            &caller.context().envelope.workspace_id,
            &mut bound,
            Some(caller),
        )?;
        if let Some(receipt) = receipt {
            if let Some(publication) = &caller.context().ordinary_child_publication {
                anyhow::ensure!(
                    Arc::ptr_eq(&receipt.memory, publication.memory()),
                    "ordinary child receipt belongs to a different memory backend"
                );
            }
            let event = receipt.event();
            anyhow::ensure!(
                event.workspace_id == caller.context().envelope.workspace_id
                    && event.source == bound.source
                    && event.owner_id == bound.owner_id
                    && event.session_key == bound.session_key
                    && event.run_id == bound.run_id
                    && event.parent_run_id == bound.parent_run_id
                    && event.parent_session_key == bound.parent_session_key
                    && event.channel == bound.channel
                    && event.sender == bound.sender
                    && event.recipient == bound.recipient
                    && event.agent_id == bound.agent_id
                    && event.persona_id == bound.persona_id
                    && event.subject == bound.subject
                    && event.causation_event_id == bound.causation_event_id
                    && event.attempt_id == bound.attempt_id
                    && event.lease_epoch == bound.lease_epoch
                    && event.config_generation_id == bound.config_generation_id
                    && event.config_source_revision == bound.config_source_revision
                    && event.visibility == bound.visibility
                    && receipt.input() == input,
                "ordinary child receipt differs from its issued source"
            );
        }
        super::turn_source::OwnedTurnSourceProof::from_ordinary_child(
            caller,
            &bound,
            input,
            receipt,
            OrdinaryChildModelSelection {
                generation: &self.generation,
                provider_name: &self.requested_provider,
                model_name: &self.requested_model,
                resolved: &self.resolved,
                route: self.route.clone(),
            },
        )
    }

    /// Produce execution lineage even when recording is disabled. The source
    /// proof is issued from the same verified caller and bound scope; no event
    /// identity is invented for a non-recorded input.
    pub(crate) fn ordinary_execution_source(
        &self,
        provider: &Arc<dyn crate::providers::Provider>,
        caller: &crate::tools::execution::ChildCallContext,
        scope: &MessageEventScope,
        input: &str,
        receipt: Option<&ChildInputReceipt>,
    ) -> anyhow::Result<(
        Arc<super::turn_source::OwnedTurnSourceProof>,
        crate::tools::ToolExecutionContext,
    )> {
        use super::turn_source::TurnSourceProof;
        let publication = caller
            .context()
            .ordinary_child_publication
            .as_ref()
            .context("ordinary child execution requires parent publication")?;
        publication.verify_caller(caller.context())?;
        let mut bound = scope.clone();
        self.bind_ordinary_input_scope(
            provider,
            &caller.context().envelope.workspace_id,
            &mut bound,
            Some(caller),
        )?;
        let proof = Arc::new(self.ordinary_source_proof(provider, caller, &bound, input, receipt)?);
        if let Some(receipt) = receipt {
            let execution = receipt.execution_context(proof.as_ref())?;
            return Ok((proof, execution));
        }
        let mut envelope = match bound.source {
            MessageEventSource::Delegate => {
                super::envelope::RuntimeEnvelope::delegate(proof.workspace_id(), proof.session_key(), proof.run_id())
            }
            MessageEventSource::SessionsSpawn => super::envelope::RuntimeEnvelope::sessions_spawn(
                proof.workspace_id(),
                proof.session_key(),
                proof.run_id(),
            ),
            _ => anyhow::bail!("unsupported ordinary child execution source"),
        };
        envelope.owner_id = Some(proof.owner_id().to_owned());
        envelope.owner_isolated = proof.owner_isolated();
        envelope.task_id = proof.task_id().map(str::to_owned);
        envelope.topic_id = match &bound.subject {
            Some(crate::memory::MessageEventSubject::Topic(topic)) => Some(topic.clone()),
            _ => None,
        };
        envelope.source_message_event_id = proof.source_event_id().map(str::to_owned);
        envelope.parent_run_id = bound.parent_run_id;
        envelope.agent_id = bound.agent_id;
        envelope.persona_id = bound.persona_id;
        envelope.channel = Some(proof.policy_channel().to_owned());
        envelope.sender = Some(proof.policy_sender().to_owned());
        envelope.recipient = bound.recipient;
        envelope.attempt_id = bound.attempt_id;
        envelope.lease_epoch = bound.lease_epoch;
        envelope.authority_guard = bound.authority_guard;
        envelope.config_generation_id = Some(proof.config_generation_id());
        envelope.config_source_revision = Some(proof.config_source_revision().to_owned());
        let execution = crate::tools::ToolExecutionContext::new(envelope, proof.policy_chat_type())
            .with_dispatch_origin(crate::tools::DispatchOrigin::ModelTurn)
            .with_incoming_child(proof.as_ref())?;
        Ok((proof, execution))
    }
}

#[cfg(test)]
mod pinned_reliability_tests {
    use super::pinned_provider_reliability;

    #[test]
    fn pinned_child_reliability_clears_both_fields_and_keeps_the_rest() {
        let base = crate::config::ReliabilityConfig {
            fallback_providers: vec!["openai".to_string()],
            api_keys: vec!["root-rotation-key".to_string()],
            provider_retries: 7,
            model_fallbacks: std::collections::HashMap::from([("k3".to_string(), vec!["k3-mini".to_string()])]),
            ..crate::config::ReliabilityConfig::default()
        };
        let pinned = pinned_provider_reliability(&base);
        assert_eq!(pinned.fallback_providers, Vec::<String>::new());
        assert_eq!(pinned.api_keys, Vec::<String>::new());
        assert_eq!(pinned.provider_retries, 7);
        assert_eq!(pinned.model_fallbacks, base.model_fallbacks);
    }
}
