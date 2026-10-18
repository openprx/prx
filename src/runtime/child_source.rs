//! Durable child-input receipts, independent of parent message provenance.
pub(crate) mod operator_process;
pub(crate) mod operator_process_input;
pub(crate) mod ordinary_process;
pub(crate) mod ordinary_task;
pub(crate) mod parent;

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

    /// Project the original durable task identity, not a worker segment envelope.
    /// This is metadata only; logical terminal issuance still verifies the grant,
    /// backend identity and every semantic field against this receipt.
    pub(crate) fn logical_task_scope(&self) -> MessageEventScope {
        let event = self.event();
        MessageEventScope {
            source: event.source.clone(),
            owner_id: event.owner_id.clone(),
            channel: event.channel.clone(),
            session_key: event.session_key.clone(),
            parent_session_key: event.parent_session_key.clone(),
            run_id: event.run_id.clone(),
            parent_run_id: event.parent_run_id.clone(),
            agent_id: event.agent_id.clone(),
            persona_id: event.persona_id.clone(),
            sender: event.sender.clone(),
            recipient: event.recipient.clone(),
            subject: event.subject.clone(),
            goal_id: event.goal_id.clone(),
            causation_event_id: event.causation_event_id.clone(),
            correlation_id: event.correlation_id.clone(),
            attempt_id: event.attempt_id.clone(),
            lease_epoch: event.lease_epoch,
            config_generation_id: event.config_generation_id,
            config_source_revision: event.config_source_revision.clone(),
            authority_guard: self.authority_guard.clone(),
            visibility: event.visibility.clone(),
        }
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
        Ok(crate::tools::ToolExecutionContext::new(
            envelope,
            proof.policy_chat_type(),
        ))
    }
}

/// Synthetic events from disabled recording cannot issue source authority.
/// The caller must require Some before entering enabled child decisions.
pub(crate) async fn record_child_input(
    fabric: &MemoryFabric,
    mut scope: MessageEventScope,
    input: &str,
    idempotency_key: Option<String>,
    payload: Option<String>,
    grant: Option<&IssuedChildGrant>,
) -> anyhow::Result<Option<Arc<ChildInputReceipt>>> {
    anyhow::ensure!(
        matches!(
            scope.source,
            MessageEventSource::SessionWorker | MessageEventSource::SessionsSpawn | MessageEventSource::Delegate
        ) && scope.visibility == MemoryVisibility::Workspace,
        "unsupported child input source"
    );
    if let Some(grant) = grant {
        anyhow::ensure!(
            fabric.workspace_id() == grant.parent().workspace_id(),
            "child input workspace differs from parent grant"
        );
        grant.bind_event_scope(&mut scope)?;
    }
    let event = fabric
        .record_inbound_user_message(scope.clone(), input, idempotency_key, payload)
        .await?;
    if event.id == 0 {
        anyhow::ensure!(grant.is_none(), "delegated child input must be durable");
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
    if let Some(grant) = grant {
        grant.verify_receipt(&receipt)?;
    }
    Ok(Some(receipt))
}

/// Recover the exact parent-recorded spawn input without appending another
/// event. This issues an input receipt only, never a source proof or grant.
pub(crate) async fn restore_worker_input(
    fabric: &MemoryFabric,
    worker: &crate::session_worker::protocol::WorkerManifest,
    scope: &crate::tools::capability::child::VerifiedChildScope,
) -> anyhow::Result<Arc<ChildInputReceipt>> {
    use crate::tools::capability::child::ChildCapabilityManifest;
    let source = worker.child_source.as_ref().context("worker child source missing")?;
    let wire = worker
        .capability_manifest
        .as_ref()
        .context("worker child capabilities missing")?;
    let (child, parent) = scope.gate_inputs();
    anyhow::ensure!(
        ChildCapabilityManifest::from_worker_manifest(wire, parent)? == *child
            && fabric.workspace_id() == child.authority.workspace
            && source.parent_constraints.workspace == child.authority.workspace
            && worker.owner_id.as_deref() == Some(child.authority.owner.as_str()),
        "worker input lookup differs from verified child scope"
    );
    let event = fabric
        .memory()
        .find_owned_message_event(fabric.workspace_id(), &child.authority.owner, &source.input_event_id)
        .await?
        .context("worker child input is not durable")?;
    verify_worker_input(fabric.workspace_id(), worker, scope, &event)?;
    Ok(Arc::new(ChildInputReceipt {
        previous_segment_input: None,
        segment_issued: false,
        memory: fabric.memory(),
        input: Arc::from(worker.task.as_str()),
        event,
        // A recovered worker authenticates through its verified manifest; a
        // parent process closure is neither durable nor transferable.
        authority_guard: None,
    }))
}

/// Check the exact durable receipt against the same authenticated manifest
/// whose execution directory will be used. A valid receipt from another worker
/// must not be combined with an independently valid transport proof.
fn verify_worker_input(
    workspace_id: &str,
    worker: &crate::session_worker::protocol::WorkerManifest,
    scope: &crate::tools::capability::child::VerifiedChildScope,
    event: &MessageEvent,
) -> anyhow::Result<()> {
    use crate::tools::capability::child::ChildCapabilityManifest;
    let source = worker.child_source.as_ref().context("worker child source missing")?;
    let wire = worker
        .capability_manifest
        .as_ref()
        .context("worker child capabilities missing")?;
    let (child, parent) = scope.gate_inputs();
    anyhow::ensure!(
        worker.ordinary_source.is_none()
            && worker.operator_source.is_none()
            && ChildCapabilityManifest::from_worker_manifest(wire, parent)? == *child
            && workspace_id == child.authority.workspace
            && source.parent_constraints.workspace == child.authority.workspace
            && worker.owner_id.as_deref() == Some(child.authority.owner.as_str()),
        "worker input differs from authenticated capability scope"
    );
    anyhow::ensure!(
        parent.key.verify_hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[
                b"worker-child-input-v1",
                child.parent_decision_id.to_string().as_bytes(),
                &serde_json::to_vec(&event)?
            ],
            &source.input_event_mac
        ),
        "worker child input differs from issued receipt"
    );
    anyhow::ensure!(
        event.id > 0
            && event.event_id == source.input_event_id
            && event.workspace_id == workspace_id
            && event.owner_id.as_deref() == Some(child.authority.owner.as_str())
            && event.source == MessageEventSource::SessionsSpawn
            && event.visibility == MemoryVisibility::Workspace
            && event.role == "user"
            && event.event_type == "message.created"
            && event.content == worker.task
            && event.run_id.as_deref() == Some(worker.run_id.as_str())
            && event.session_key.as_deref() == Some(worker.session_scope_key.as_str())
            && event.parent_run_id == worker.parent_run_id
            && event.run_id != event.parent_run_id
            && event.parent_session_key.as_deref().is_some_and(|id| !id.is_empty())
            && event.causation_event_id.as_deref().is_some_and(|id| !id.is_empty())
            && event.config_generation_id == worker.runtime_config_generation_id
            && event.config_source_revision == worker.runtime_config_source_revision
            && event.channel == worker.scope_channel
            && event.sender == worker.scope_sender
            && event.agent_id == worker.agent_id,
        "worker durable input differs from manifest lineage"
    );
    Ok(())
}

/// Durable input identity for each execution segment of one logical child task.
/// The task's own final result keeps its original run id; segment decisions
/// and terminals use disjoint ids, including when steering cancels a segment.
pub(crate) struct ChildSegmentSequence {
    grant: Arc<IssuedChildGrant>,
    task_input: Arc<ChildInputReceipt>,
    segment_source: MessageEventSource,
    next: u64,
    previous_input: Option<SegmentPredecessor>,
}

impl ChildSegmentSequence {
    pub(crate) fn new(grant: Arc<IssuedChildGrant>, task_input: Arc<ChildInputReceipt>) -> anyhow::Result<Self> {
        grant.verify_receipt(&task_input)?;
        Ok(Self {
            grant,
            segment_source: task_input.event().source.clone(),
            task_input,
            next: 0,
            previous_input: None,
        })
    }

    pub(crate) const fn recorded_count(&self) -> u64 {
        self.next
    }

    pub(crate) fn new_worker(grant: Arc<IssuedChildGrant>, task_input: Arc<ChildInputReceipt>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            task_input.event().source == MessageEventSource::SessionsSpawn,
            "worker segments require the original spawn input"
        );
        let mut sequence = Self::new(grant, task_input)?;
        sequence.segment_source = MessageEventSource::SessionWorker;
        Ok(sequence)
    }

    pub(crate) async fn record(
        &mut self,
        fabric: &MemoryFabric,
        input: &str,
    ) -> anyhow::Result<Arc<ChildInputReceipt>> {
        let following = self.next.checked_add(1).context("child segment identity exhausted")?;
        let event = self.task_input.event();
        let task_run = event.run_id.as_deref().context("child task run missing")?;
        let segment_run = format!("child-segment:{}:{}", event.event_id, self.next);
        let scope = MessageEventScope {
            source: self.segment_source.clone(),
            owner_id: event.owner_id.clone(),
            channel: event.channel.clone(),
            session_key: event.session_key.clone(),
            parent_session_key: event.parent_session_key.clone(),
            run_id: Some(segment_run.clone()),
            parent_run_id: event.parent_run_id.clone(),
            agent_id: event.agent_id.clone(),
            persona_id: event.persona_id.clone(),
            sender: event.sender.clone(),
            recipient: event.recipient.clone(),
            subject: Some(crate::memory::MessageEventSubject::Task(task_run.to_owned())),
            goal_id: event.goal_id.clone(),
            causation_event_id: event.causation_event_id.clone(),
            correlation_id: event.correlation_id.clone().or_else(|| Some(event.event_id.clone())),
            attempt_id: event.attempt_id.clone(),
            lease_epoch: event.lease_epoch,
            config_generation_id: event.config_generation_id,
            config_source_revision: event.config_source_revision.clone(),
            authority_guard: self.task_input.authority_guard.clone(),
            visibility: event.visibility.clone(),
        };
        let (manifest, _) = self.grant.scope().gate_inputs();
        let payload = serde_json::json!({
            "task_run_id": task_run,
            "task_input_event_id": event.event_id,
            "segment_index": self.next,
            "previous_segment_input_event_id": self.previous_input.as_ref().map(SegmentPredecessor::event_id),
            "parent_decision_id": manifest.parent_decision_id.to_string(),
        })
        .to_string();
        let mut receipt = record_child_input(
            fabric,
            scope,
            input,
            Some(format!("{segment_run}:input")),
            Some(payload),
            Some(&self.grant),
        )
        .await?
        .context("child segment input must be durable")?;
        // A failed/cancelled write retries the same identity; it cannot skip an
        // input or turn a changed body into a successful idempotent receipt.
        let next_previous = ChildInputReceipt::advance_segment(&mut receipt, self.previous_input.as_ref())?;
        self.next = following;
        self.previous_input = Some(next_previous);
        Ok(receipt)
    }
}

/// Source-bound issuance retained across the background child's lifetime.
/// Only runtime admission can mint it; tools may carry or inspect it.
#[derive(Clone)]
pub(crate) struct IssuedChildGrant(Arc<ChildGrantData>);

pub(crate) struct ChildGrantRuntime {
    pub(crate) decision_store: Arc<crate::causal_tree::store::CausalDecisionStore>,
    pub(crate) source_fabric: MemoryFabric,
    pub(crate) security: Arc<crate::security::SecurityPolicy>,
}

#[derive(Clone)]
struct ParentModelSelection {
    provider: String,
    model: String,
    credential: crate::providers::credentials::CredentialRef,
}

struct ChildGrantData {
    // Only a live parent decision can issue default child-model selection.
    // Restored incoming authority is used to admit that child, not to spawn
    // another child without a fresh decision-issued outgoing grant.
    parent_model: Option<ParentModelSelection>,
    source_fabric: MemoryFabric,
    security: Arc<crate::security::SecurityPolicy>,
    parent_record_binding: (Arc<str>, String),
    provider_catalog: Arc<crate::providers::catalog::CatalogView>,
    catalog: Arc<crate::tools::capability::catalog::CapabilityCatalog>,
    generation: Arc<crate::config::ConfigGeneration>,
    scope: Arc<crate::tools::capability::child::VerifiedChildScope>,
    parent: parent::ParentSource,
}

impl std::fmt::Debug for IssuedChildGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedChildGrant").finish_non_exhaustive()
    }
}
impl PartialEq for IssuedChildGrant {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for IssuedChildGrant {}

impl IssuedChildGrant {
    pub(super) fn new(
        scope: Arc<crate::tools::capability::child::VerifiedChildScope>,
        parent: &dyn crate::runtime::turn_source::TurnSourceProof,
        catalog: Arc<crate::tools::capability::catalog::CapabilityCatalog>,
        generation: Arc<crate::config::ConfigGeneration>,
        provider_catalog: Arc<crate::providers::catalog::CatalogView>,
        parent_record_binding: (Arc<str>, String),
        execution_policy: super::turn_decision::ChildExecutionPolicy,
        runtime: ChildGrantRuntime,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            execution_policy.matches(&runtime.security),
            "child execution policy differs from admitted service"
        );
        let execution_security = Arc::clone(&runtime.security);
        Self::from_parent(
            scope,
            (
                parent::ParentSource::capture(parent),
                Some(ParentModelSelection {
                    provider: parent.provider_name().to_owned(),
                    model: parent.model_name().to_owned(),
                    credential: parent.model_binding().credential_reference().clone(),
                }),
            ),
            catalog,
            generation,
            provider_catalog,
            parent_record_binding,
            runtime,
            &execution_security.workspace_dir,
        )
    }

    fn from_parent(
        scope: Arc<crate::tools::capability::child::VerifiedChildScope>,
        source: (parent::ParentSource, Option<ParentModelSelection>),
        catalog: Arc<crate::tools::capability::catalog::CapabilityCatalog>,
        generation: Arc<crate::config::ConfigGeneration>,
        provider_catalog: Arc<crate::providers::catalog::CatalogView>,
        parent_record_binding: (Arc<str>, String),
        runtime: ChildGrantRuntime,
        execution_workspace: &std::path::Path,
    ) -> anyhow::Result<Self> {
        let (parent, parent_model) = source;
        let ChildGrantRuntime {
            decision_store,
            source_fabric,
            security,
        } = runtime;
        let actual_store = decision_store
            .workspace_id()
            .context("child decision store namespace missing")?;
        let (manifest, _) = scope.gate_inputs();
        anyhow::ensure!(
            source_fabric.workspace_id() == parent.workspace_id()
                && security.workspace_dir == execution_workspace
                && manifest.catalog_generation == catalog.view(&Default::default()).generation()
                && parent_record_binding.0 == actual_store
                && parent_record_binding.1.len() == 64
                && parent_record_binding.1.bytes().all(|byte| byte.is_ascii_hexdigit())
                && generation.id.0 == parent.config_generation_id()
                && generation.source_revision.fingerprint_sha256 == parent.config_source_revision()
                && generation.effective.workspace_dir.to_string_lossy() == parent.workspace_id()
                && manifest.authority.owner == parent.owner_id()
                && manifest.authority.workspace == parent.workspace_id()
                && !parent.run_id().is_empty()
                && !parent.session_key().is_empty()
                && parent.source_event_id().is_some_and(|id| !id.is_empty()),
            "child grant differs from admitted parent source"
        );
        Ok(Self(Arc::new(ChildGrantData {
            parent_model,
            source_fabric,
            security,
            parent_record_binding,
            provider_catalog,
            catalog,
            generation,
            scope,
            parent,
        })))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restore_process(
        scope: Arc<crate::tools::capability::child::VerifiedChildScope>,
        parent: parent::ParentSource,
        catalog: Arc<crate::tools::capability::catalog::CapabilityCatalog>,
        generation: Arc<crate::config::ConfigGeneration>,
        provider_catalog: Arc<crate::providers::catalog::CatalogView>,
        parent_record_binding: (Arc<str>, String),
        runtime: ChildGrantRuntime,
        receipt: &ChildInputReceipt,
        worker: &crate::session_worker::protocol::WorkerManifest,
        transport: &crate::session_worker::runner::AuthenticatedWorkerTransport,
    ) -> anyhow::Result<Arc<Self>> {
        transport.verify_manifest(worker)?;
        receipt.verify_backend(&runtime.source_fabric.memory())?;
        verify_worker_input(runtime.source_fabric.workspace_id(), worker, &scope, receipt.event())?;
        let source = worker.child_source.as_ref().context("worker child source missing")?;
        let (child, constraints) = scope.gate_inputs();
        let expected_parent = parent.bind_process(child.parent_decision_id, &constraints.key)?;
        anyhow::ensure!(
            source.parent_store_workspace == parent_record_binding.0.as_ref()
                && source.parent_decision_digest == parent_record_binding.1
                && source.parent_source.matches_binding(&expected_parent),
            "worker restore arguments differ from authenticated manifest provenance"
        );
        let grant = Self::from_parent(
            scope,
            (parent, None),
            catalog,
            generation,
            provider_catalog,
            parent_record_binding,
            runtime,
            &worker.workspace_dir,
        )?;
        grant.verify_receipt(receipt)?;
        Ok(Arc::new(grant))
    }

    /// Attach only authority already issued to this exact durable spawn input.
    /// The caller signs the complete worker manifest after this method returns.
    pub(crate) fn bind_worker_manifest(
        &self,
        worker: &mut crate::session_worker::protocol::WorkerManifest,
        receipt: &ChildInputReceipt,
        selection: &crate::session_worker::protocol::WorkerProviderSelection,
    ) -> anyhow::Result<()> {
        self.verify_receipt(receipt)?;
        let event = receipt.event();
        let parent = self.parent();
        let (manifest, constraints) = self.scope().gate_inputs();
        manifest.verify(constraints)?;
        anyhow::ensure!(
            worker.capability_manifest.is_none()
                && worker.ordinary_source.is_none()
                && worker.child_source.is_none()
                && worker.parent_capability.is_none()
                && event.source == MessageEventSource::SessionsSpawn
                && receipt.input() == worker.task
                && event.run_id.as_deref() == Some(worker.run_id.as_str())
                && event.session_key.as_deref() == Some(worker.session_scope_key.as_str())
                && worker.owner_id.as_deref() == Some(parent.owner_id())
                && worker.parent_run_id.as_deref() == Some(parent.run_id())
                && worker.runtime_config_generation_id == Some(parent.config_generation_id())
                && worker.runtime_config_source_revision.as_deref() == Some(parent.config_source_revision())
                && worker.scope_channel.as_deref() == Some(parent.policy_channel())
                && worker.scope_sender.as_deref() == Some(parent.policy_sender())
                && worker.scope_chat_type.as_deref() == Some(parent.policy_chat_type())
                && worker.agent_id == event.agent_id,
            "worker manifest differs from issued child source"
        );
        let (workspace, digest) = self.parent_record_binding();
        let parent_constraints = constraints.to_worker_constraints()?;
        let config_generation = self.generation().bind_process(&constraints.key)?;
        let catalog = self.catalog().bind_process(&constraints.key)?;
        let provider_catalog = self
            .provider_catalog()
            .bind_process(&constraints.key, manifest.parent_decision_id)?;
        let parent_source = self
            .0
            .parent
            .bind_process(manifest.parent_decision_id, &constraints.key)?;
        let input_event_mac = constraints.key.hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[
                b"worker-child-input-v1",
                manifest.parent_decision_id.to_string().as_bytes(),
                &serde_json::to_vec(event)?,
            ],
        );
        let resolved = crate::providers::selection::check_with(
            &self.generation().effective,
            self.provider_catalog(),
            &selection.provider,
            &selection.model,
            selection.credential.clone(),
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        anyhow::ensure!(
            crate::providers::ProviderId::of(resolved.provider_label())
                == crate::providers::ProviderId::of(&worker.provider_name)
                && resolved.model() == worker.model,
            "worker selected provider differs from issued model request"
        );
        let route =
            crate::providers::selection::route_target(self.provider_catalog(), &selection.provider, &selection.model)?;
        anyhow::ensure!(
            route.map(|route| route.hint.as_str()) == worker.route_hint.as_deref(),
            "worker selected route differs from issued model request"
        );
        let root = matches!(
            resolved.credential().reference(),
            crate::providers::credentials::CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root)
        );
        worker.api_key = resolved.credential().api_key().map(str::to_owned);
        worker.direct_uses_root_connection = route.is_none() && root;
        worker.route_uses_root_connection = route.is_some() && root;
        worker.route_source_provider = route.map(|_| selection.provider.clone());
        worker.route_credential = route.map(|_| resolved.credential().reference().clone());
        worker.capability_manifest = Some(manifest.to_worker_manifest());
        worker.child_source = Some(crate::session_worker::protocol::WorkerChildSource {
            input_event_id: event.event_id.clone(),
            provider_selection: selection.clone(),
            input_event_mac,
            parent_source,
            provider_catalog,
            parent_store_workspace: workspace.to_owned(),
            parent_decision_digest: digest.to_owned(),
            parent_constraints,
            config_generation,
            catalog,
        });
        Ok(())
    }

    pub(crate) fn provider_catalog(&self) -> &crate::providers::catalog::CatalogView {
        &self.0.provider_catalog
    }

    pub(crate) fn catalog(&self) -> &Arc<crate::tools::capability::catalog::CapabilityCatalog> {
        &self.0.catalog
    }

    pub(crate) fn generation(&self) -> &Arc<crate::config::ConfigGeneration> {
        &self.0.generation
    }

    pub(crate) fn scope(&self) -> &Arc<crate::tools::capability::child::VerifiedChildScope> {
        &self.0.scope
    }

    /// A process worker cannot own tools whose backend lives only in the
    /// interactive parent's event loop. Re-derive a sibling child scope from
    /// the same authenticated parent so the transport carries only executable
    /// worker capabilities, without consuming another delegation depth.
    pub(crate) fn for_process_worker(&self) -> anyhow::Result<Self> {
        let (manifest, parent) = self.scope().gate_inputs();
        let catalog = self.catalog().view(&Default::default());
        let requested = manifest
            .allowed
            .iter()
            .filter(|reference| {
                catalog
                    .entry_any(&reference.id)
                    .is_some_and(|entry| entry.process_worker_compatible)
            })
            .cloned()
            .collect::<Vec<_>>();
        if requested.len() == manifest.allowed.len() {
            return Ok(self.clone());
        }
        let narrowed =
            crate::tools::capability::child::ChildCapabilityManifest::derive_strict_subset(parent, &requested)?;
        let scope = Arc::new(crate::tools::capability::child::VerifiedChildScope::new(
            narrowed, parent,
        )?);
        Ok(Self(Arc::new(ChildGrantData {
            parent_model: self.0.parent_model.clone(),
            source_fabric: self.0.source_fabric.clone(),
            security: Arc::clone(&self.0.security),
            parent_record_binding: self.0.parent_record_binding.clone(),
            provider_catalog: Arc::clone(&self.0.provider_catalog),
            catalog: Arc::clone(&self.0.catalog),
            generation: Arc::clone(&self.0.generation),
            scope,
            parent: self.0.parent.clone(),
        })))
    }

    pub(crate) fn source_fabric(&self) -> &MemoryFabric {
        &self.0.source_fabric
    }

    pub(crate) fn security(&self) -> &Arc<crate::security::SecurityPolicy> {
        &self.0.security
    }

    pub(crate) fn model_source(&self) -> anyhow::Result<(&str, &str, &crate::providers::credentials::CredentialRef)> {
        let source = self
            .0
            .parent_model
            .as_ref()
            .context("restored incoming grant cannot select a new child model")?;
        Ok((&source.provider, &source.model, &source.credential))
    }

    pub(crate) fn parent_record_binding(&self) -> (&str, &str) {
        (&self.0.parent_record_binding.0, &self.0.parent_record_binding.1)
    }

    pub(crate) fn parent(&self) -> &parent::ParentSource {
        &self.0.parent
    }

    /// Fill absent lineage only from the issued parent; contradictory existing
    /// fields are errors rather than silently rewritten authority.
    pub(crate) fn bind_event_scope(&self, scope: &mut MessageEventScope) -> anyhow::Result<()> {
        let parent = self.parent();
        anyhow::ensure!(
            scope
                .run_id
                .as_deref()
                .is_some_and(|id| !id.is_empty() && id != parent.run_id())
                && scope.session_key.as_deref().is_some_and(|id| !id.is_empty()),
            "child input requires a distinct run and a session"
        );
        for (field, expected) in [
            (&mut scope.owner_id, parent.owner_id()),
            (&mut scope.parent_run_id, parent.run_id()),
            (&mut scope.parent_session_key, parent.session_key()),
            (
                &mut scope.causation_event_id,
                parent.source_event_id().context("parent source missing")?,
            ),
            (&mut scope.channel, parent.policy_channel()),
            (&mut scope.sender, parent.policy_sender()),
            (&mut scope.config_source_revision, parent.config_source_revision()),
        ] {
            anyhow::ensure!(
                field.as_deref().is_none_or(|actual| actual == expected),
                "child input lineage contradicts issued parent"
            );
            if field.is_none() {
                *field = Some(expected.to_owned());
            }
        }
        anyhow::ensure!(
            scope
                .config_generation_id
                .is_none_or(|id| id == parent.config_generation_id()),
            "child input generation contradicts issued parent"
        );
        scope.config_generation_id = Some(parent.config_generation_id());
        Ok(())
    }

    pub(crate) fn verify_receipt(&self, receipt: &ChildInputReceipt) -> anyhow::Result<()> {
        let parent = self.parent();
        let event = receipt.event();
        anyhow::ensure!(
            event.workspace_id == parent.workspace_id()
                && event.owner_id.as_deref() == Some(parent.owner_id())
                && event.parent_run_id.as_deref() == Some(parent.run_id())
                && event
                    .run_id
                    .as_deref()
                    .is_some_and(|id| !id.is_empty() && id != parent.run_id())
                && event.parent_session_key.as_deref() == Some(parent.session_key())
                && event.causation_event_id.as_deref() == parent.source_event_id()
                && event.channel.as_deref() == Some(parent.policy_channel())
                && event.sender.as_deref() == Some(parent.policy_sender())
                && event.config_generation_id == Some(parent.config_generation_id())
                && event.config_source_revision.as_deref() == Some(parent.config_source_revision()),
            "child input does not descend from the issued parent decision"
        );
        Ok(())
    }
}

// None deliberately shadows ambient authority on every service dispatch.
tokio::task_local! {
    static OUTGOING_GRANT: Option<Arc<IssuedChildGrant>>;
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

pub(crate) async fn scope_outgoing_grant<F: std::future::Future>(
    grant: Option<Arc<IssuedChildGrant>>,
    work: F,
) -> F::Output {
    OUTGOING_GRANT.scope(grant, work).await
}

pub(crate) fn outgoing_grant() -> Option<Arc<IssuedChildGrant>> {
    OUTGOING_GRANT.try_with(Clone::clone).ok().flatten()
}

/// The exact PMM selection used by the child provider factory. Fields are
/// runtime-private; the child obtains it only for the actual provider Arc.
pub(crate) struct ChildModelSelection<'a> {
    pub(super) grant: &'a IssuedChildGrant,
    pub(super) provider_name: &'a str,
    pub(super) model_name: &'a str,
    pub(super) resolved: &'a crate::providers::catalog::ResolvedModel,
    pub(super) route: Option<crate::providers::catalog::RouteTarget>,
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
    grant: Option<Arc<IssuedChildGrant>>,
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
    pub(crate) fn build(
        grant: Arc<IssuedChildGrant>,
        provider_name: &str,
        model_name: &str,
        credential: crate::providers::credentials::CredentialRef,
    ) -> anyhow::Result<Self> {
        let config = &grant.generation().effective;
        let catalog = grant.provider_catalog();
        let route = crate::providers::selection::route_target(catalog, provider_name, model_name)?.cloned();
        let resolved = crate::providers::selection::check_with(
            config,
            catalog,
            provider_name,
            model_name,
            credential,
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        Self::from_selection(
            Some(Arc::clone(&grant)),
            Arc::clone(grant.generation()),
            catalog,
            provider_name,
            model_name,
            resolved,
            route,
        )
    }

    /// Preserve the provider instance authenticated by the explicit operator
    /// ingress. This model binding alone does not grant child capabilities.
    pub(crate) fn build_operator(source: &super::operator_spawn::OperatorSpawnSource) -> anyhow::Result<Self> {
        use super::turn_source::TurnSourceProof;
        source.verify_live()?;
        let binding = Self {
            grant: None,
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
            self.grant.is_none()
                && Arc::ptr_eq(&self.generation, source.generation())
                && Arc::ptr_eq(&self.provider, source.provider())
                && self.requested_provider == source.proof().provider_name()
                && self.requested_model == source.proof().model_name()
                && source.proof().model_binding().matches(&self.resolved)
                && self.route.as_ref() == source.route(),
            "operator child model binding differs from pinned source"
        );
        Ok(())
    }

    pub(crate) fn build_worker(
        grant: Arc<IssuedChildGrant>,
        worker: &crate::session_worker::protocol::WorkerManifest,
    ) -> anyhow::Result<Self> {
        let selection = &worker
            .child_source
            .as_ref()
            .context("worker provider selection missing")?
            .provider_selection;
        let catalog = grant.provider_catalog();
        let route = crate::providers::selection::route_target(catalog, &selection.provider, &selection.model)?.cloned();
        let resolved = crate::providers::selection::check_with(
            &grant.generation().effective,
            catalog,
            &selection.provider,
            &selection.model,
            selection.credential.clone(),
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        let root = matches!(
            resolved.credential().reference(),
            crate::providers::credentials::CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root)
        );
        anyhow::ensure!(
            crate::providers::ProviderId::of(resolved.provider_label())
                == crate::providers::ProviderId::of(&worker.provider_name)
                && resolved.model() == worker.model
                && resolved.credential().api_key() == worker.api_key.as_deref()
                && route.as_ref().map(|route| route.hint.as_str()) == worker.route_hint.as_deref()
                && worker.direct_uses_root_connection == (route.is_none() && root)
                && worker.route_uses_root_connection == (route.is_some() && root)
                && worker.route_source_provider.as_deref() == route.as_ref().map(|_| selection.provider.as_str())
                && worker.route_credential.as_ref() == route.as_ref().map(|_| resolved.credential().reference()),
            "worker provider or credential provenance differs from issued selection"
        );
        Self::from_selection(
            Some(Arc::clone(&grant)),
            Arc::clone(grant.generation()),
            catalog,
            &selection.provider,
            &selection.model,
            resolved,
            route,
        )
    }

    /// A disabled ordinary child has model provenance, but no parent authority.
    pub(crate) fn build_ordinary(
        config: &crate::config::Config,
        catalog: &crate::providers::catalog::CatalogView,
        provider_name: &str,
        model_name: &str,
        credential: crate::providers::credentials::CredentialRef,
        publication: Option<&super::turn_decision::OrdinaryChildPublication>,
    ) -> anyhow::Result<Self> {
        let generation = match publication {
            Some(publication) => Arc::clone(publication.generation()),
            None => crate::config::ConfigGeneration::for_disabled_child(config)?,
        };
        let catalog = publication.map_or(catalog, |publication| publication.provider_catalog());
        anyhow::ensure!(
            !generation.effective.causal_tree.enabled,
            "ordinary child requires disabled decision plane"
        );
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
                grant: None,
                generation,
                provider,
                requested_provider: provider_name.to_owned(),
                requested_model: model_name.to_owned(),
                resolved,
                route,
            });
        }
        Self::from_selection(None, generation, catalog, provider_name, model_name, resolved, route)
    }

    fn from_selection(
        grant: Option<Arc<IssuedChildGrant>>,
        generation: Arc<crate::config::ConfigGeneration>,
        catalog: &crate::providers::catalog::CatalogView,
        provider_name: &str,
        model_name: &str,
        resolved: crate::providers::catalog::ResolvedModel,
        route: Option<crate::providers::catalog::RouteTarget>,
    ) -> anyhow::Result<Self> {
        let config = &generation.effective;
        // A delegated provider remains pinned to its selected vendor, matching
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
            grant,
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
            self.grant.is_none()
                && !self.generation.effective.causal_tree.enabled
                && Arc::ptr_eq(provider, &self.provider)
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
                    caller.context().execution_bindings.is_none(),
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
        let execution = crate::tools::ToolExecutionContext::new(envelope, proof.policy_chat_type());
        Ok((proof, execution))
    }

    pub(crate) fn source_proof(
        &self,
        grant: &IssuedChildGrant,
        receipt: &ChildInputReceipt,
        provider: &Arc<dyn crate::providers::Provider>,
    ) -> anyhow::Result<super::turn_source::OwnedTurnSourceProof> {
        super::turn_source::OwnedTurnSourceProof::from_child_receipt(
            grant,
            receipt,
            &self.generation,
            self.selection_for(provider)?,
        )
    }

    pub(crate) fn selection_for(
        &self,
        provider: &Arc<dyn crate::providers::Provider>,
    ) -> anyhow::Result<ChildModelSelection<'_>> {
        anyhow::ensure!(
            Arc::ptr_eq(provider, &self.provider),
            "child provider instance changed after selection"
        );
        Ok(ChildModelSelection {
            grant: self
                .grant
                .as_deref()
                .context("ordinary child has no delegated parent authority")?,
            provider_name: &self.requested_provider,
            model_name: &self.requested_model,
            resolved: &self.resolved,
            route: self.route.clone(),
        })
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
