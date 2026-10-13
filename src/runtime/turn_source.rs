//! Closed source-proof contract shared by admitted model turn adapters.
//! Only ingress-owned opaque proofs implement the private sealing trait.
use crate::agent::entry::{AgentModelBinding, AgentTurnProof};
use crate::causal_tree::state::TurnSurfaceKind;
use crate::memory::principal::MemoryWriteContext;

/// Persistence shape for supported ingress adapters. This recognizes a
/// format only; callers must separately authenticate lineage and signatures.
pub(crate) fn ingress_event_surface(
    source: &crate::memory::MessageEventSource,
    channel: Option<&str>,
    visibility: &crate::memory::MemoryVisibility,
) -> Option<TurnSurfaceKind> {
    use crate::memory::{MemoryVisibility, MessageEventSource};
    match (source, channel, visibility) {
        (MessageEventSource::Agent, Some("cli" | "agent_api"), MemoryVisibility::Session) => {
            Some(TurnSurfaceKind::AgentLoop)
        }
        (MessageEventSource::Chat, Some("terminal"), MemoryVisibility::Workspace) => Some(TurnSurfaceKind::Interactive),
        (MessageEventSource::Gateway | MessageEventSource::Console, Some(channel), MemoryVisibility::Session)
            if !channel.is_empty() =>
        {
            Some(TurnSurfaceKind::Server)
        }
        (MessageEventSource::Channel, Some(channel), MemoryVisibility::Session | MemoryVisibility::Workspace)
            if !channel.is_empty() =>
        {
            Some(TurnSurfaceKind::Channel)
        }
        (MessageEventSource::Cron, Some(channel), MemoryVisibility::Workspace) if !channel.is_empty() => {
            Some(TurnSurfaceKind::Cron)
        }
        (MessageEventSource::Xin, Some(channel), MemoryVisibility::Workspace) if !channel.is_empty() => {
            Some(TurnSurfaceKind::Xin)
        }
        (
            MessageEventSource::SessionWorker | MessageEventSource::SessionsSpawn | MessageEventSource::Delegate,
            Some(channel),
            MemoryVisibility::Workspace,
        ) if !channel.is_empty() => Some(TurnSurfaceKind::Worker),
        _ => None,
    }
}

/// Select the persistence transform from the service-issued surface, never
/// from an untrusted marker field. The raw routing input remains in memory.
pub(crate) fn recorded_input_matches(
    surface: TurnSurfaceKind,
    event: &crate::memory::MessageEvent,
    raw_input: &str,
) -> bool {
    if ingress_event_surface(&event.source, event.channel.as_deref(), &event.visibility) != Some(surface)
        || (surface == TurnSurfaceKind::AgentLoop
            && !matches!(
                (event.channel.as_deref(), event.sender.as_deref()),
                (Some("cli"), Some("local-user")) | (Some("agent_api"), Some("embedded-caller"))
            ))
        || (surface == TurnSurfaceKind::Interactive && event.sender.as_deref() != Some("local-user"))
    {
        return false;
    }
    match surface {
        TurnSurfaceKind::AgentLoop
        | TurnSurfaceKind::Server
        | TurnSurfaceKind::Cron
        | TurnSurfaceKind::Xin
        | TurnSurfaceKind::Worker => event.content == raw_input,
        TurnSurfaceKind::Channel => {
            event.content == crate::agent::turn_contracts::inbound_user_message(raw_input).content
        }
        TurnSurfaceKind::Interactive => event.content == crate::chat::sanitize::sanitize_for_persistence(raw_input),
    }
}

/// A task subject is distinct from a topic or goal. Do not infer it from a
/// run id or from free-form event payloads.
pub(crate) const fn subject_task_id(subject: Option<&crate::memory::MessageEventSubject>) -> Option<&str> {
    match subject {
        Some(crate::memory::MessageEventSubject::Task(task_id)) => Some(task_id.as_str()),
        _ => None,
    }
}

mod sealed {
    impl Sealed for super::OwnedTurnSourceProof {}
    impl Sealed for crate::runtime::operator_spawn::task::OperatorTaskAuthority {}
    pub trait Sealed {}
    impl Sealed for crate::agent::entry::AgentTurnProof {}
    impl Sealed for crate::agent::embedded_source::EmbeddedAgentSource {}
    impl Sealed for crate::chat::decision_source::ChatTurnBinding {}
    impl Sealed for crate::gateway::decision_source::GatewayTurnProof {}
    impl Sealed for crate::channels::decision_source::ChannelTurnProof {}
}

pub(crate) trait TurnSourceProof: sealed::Sealed + Send + Sync {
    /// Execution isolation from the actual sealed origin, never caller arguments.
    fn owner_isolated(&self) -> bool;
    /// Only the actual authenticated owner-review ingress opts in.
    fn approval_review_adapter(&self) -> Option<crate::causal_tree::approval_collection::ApprovalReviewAdapter> {
        None
    }
    /// Locally issued operator task identity, never a parent model decision.
    fn operator_task_authority_id(&self) -> Option<uuid::Uuid> {
        None
    }
    fn operator_process_authority_id(&self) -> Option<uuid::Uuid> {
        None
    }
    /// Process-local task issuance identity; never loaded from wire metadata.
    fn ordinary_task_authority_id(&self) -> Option<uuid::Uuid> {
        None
    }
    /// Runtime-issued worker identity, never deserialized from a manifest.
    fn ordinary_process_authority_id(&self) -> Option<uuid::Uuid> {
        None
    }
    /// Ingress configuration may narrow a model's capabilities, never add
    /// them. Owned child proofs derive this from authenticated source metadata.
    fn tool_calls_allowed(&self) -> bool {
        true
    }
    /// Immutable model-turn restriction captured by the actual ingress.
    fn intercepts_writes(&self) -> bool {
        false
    }
    /// Whether this source belongs to a conversation with an earlier turn.
    fn has_conversation_history(&self) -> bool {
        false
    }
    fn child_scope(&self) -> Option<&std::sync::Arc<crate::tools::capability::child::VerifiedChildScope>> {
        None
    }
    /// Exact source-owned read identity, required from every sealed ingress.
    /// Child issuers must not substitute an inherited parent session here.
    fn source_memory_principal(&self) -> crate::memory::MemoryPrincipal;
    fn workspace_id(&self) -> &str;
    fn owner_id(&self) -> &str;
    fn session_key(&self) -> &str;
    fn run_id(&self) -> &str;
    fn task_id(&self) -> Option<&str> {
        None
    }
    fn routing_input(&self) -> &str;
    fn provider_name(&self) -> &str;
    fn model_name(&self) -> &str;
    fn config_source_revision(&self) -> &str;
    fn source_event_id(&self) -> Option<&str>;
    fn memory_scope(&self) -> &MemoryWriteContext;
    fn model_binding(&self) -> &AgentModelBinding;
    fn config_generation_id(&self) -> u64;
    fn surface(&self) -> TurnSurfaceKind;
    fn policy_channel(&self) -> &str;
    fn policy_sender(&self) -> &str {
        "local-user"
    }
    fn policy_chat_type(&self) -> &str {
        "private"
    }
}

impl TurnSourceProof for AgentTurnProof {
    fn owner_isolated(&self) -> bool {
        Self::owner_isolated(self)
    }
    fn source_memory_principal(&self) -> crate::memory::MemoryPrincipal {
        self.retrieval_principal()
    }

    fn workspace_id(&self) -> &str {
        Self::workspace_id(self)
    }
    fn owner_id(&self) -> &str {
        Self::owner_id(self)
    }
    fn session_key(&self) -> &str {
        Self::session_key(self)
    }
    fn run_id(&self) -> &str {
        Self::run_id(self)
    }
    fn routing_input(&self) -> &str {
        Self::routing_input(self)
    }
    fn provider_name(&self) -> &str {
        Self::provider_name(self)
    }
    fn model_name(&self) -> &str {
        Self::model_name(self)
    }
    fn config_source_revision(&self) -> &str {
        Self::config_source_revision(self)
    }
    fn source_event_id(&self) -> Option<&str> {
        Self::source_event_id(self)
    }
    fn memory_scope(&self) -> &MemoryWriteContext {
        Self::memory_scope(self)
    }
    fn model_binding(&self) -> &AgentModelBinding {
        Self::model_binding(self)
    }
    fn config_generation_id(&self) -> u64 {
        Self::config_generation_id(self)
    }
    fn surface(&self) -> TurnSurfaceKind {
        Self::surface(self)
    }
    fn task_id(&self) -> Option<&str> {
        Self::task_id(self)
    }
    fn policy_channel(&self) -> &str {
        Self::policy_channel(self)
    }
    fn policy_sender(&self) -> &str {
        Self::policy_sender(self)
    }
    fn policy_chat_type(&self) -> &str {
        Self::policy_chat_type(self)
    }
}

/// An owned sealed source proof, captured for delayed execution or constructed
/// from a durable child receipt and a runtime-issued parent grant.
/// Neither fields nor deserialization can synthesize source authority.
pub(crate) struct OwnedTurnSourceProof {
    owner_isolated: bool,
    approval_review_adapter: Option<crate::causal_tree::approval_collection::ApprovalReviewAdapter>,
    previous_segment_input: Option<String>,
    segment_input_sequence: Option<i64>,
    operator_task_authority_id: Option<uuid::Uuid>,
    operator_process_authority_id: Option<uuid::Uuid>,
    ordinary_task_authority_id: Option<uuid::Uuid>,
    ordinary_process_authority_id: Option<uuid::Uuid>,
    tool_calls_allowed: bool,
    intercepts_writes: bool,
    has_conversation_history: bool,
    child_scope: Option<std::sync::Arc<crate::tools::capability::child::VerifiedChildScope>>,
    workspace_id: String,
    owner_id: String,
    session_key: String,
    run_id: String,
    task_id: Option<String>,
    routing_input: String,
    provider_name: String,
    model_name: String,
    config_source_revision: String,
    source_event_id: Option<String>,
    memory_scope: MemoryWriteContext,
    source_memory_principal: crate::memory::MemoryPrincipal,
    model_binding: AgentModelBinding,
    config_generation_id: u64,
    surface: TurnSurfaceKind,
    policy_channel: String,
    policy_sender: String,
    policy_chat_type: String,
}

/// The caller must verify the issued child scope before projecting it.
/// A new child session never inherits the parent's legacy-session alias.
fn child_memory_principal(
    workspace: &str,
    scope: &crate::memory::MessageEventScope,
    require_owner_match: bool,
) -> anyhow::Result<crate::memory::MemoryPrincipal> {
    use anyhow::Context;
    let owner = scope
        .owner_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .context("child retrieval owner missing")?;
    let session = scope
        .session_key
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .context("child retrieval session missing")?;
    Ok(crate::memory::MemoryPrincipal {
        workspace_id: workspace.to_owned(),
        owner_id: Some(owner.to_owned()),
        session_key: Some(session.to_owned()),
        agent_id: scope.agent_id.clone(),
        persona_id: scope.persona_id.clone(),
        channel: scope.channel.clone(),
        sender: scope.sender.clone(),
        require_owner_match,
        legacy_session_key: None,
    })
}

impl OwnedTurnSourceProof {
    pub(super) fn previous_segment_input(&self) -> Option<&str> {
        self.previous_segment_input.as_deref()
    }
    pub(super) const fn segment_input_sequence(&self) -> Option<i64> {
        self.segment_input_sequence
    }

    pub(super) fn bind_ordinary_task(&mut self, authority_id: uuid::Uuid) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.operator_task_authority_id.is_none()
                && self.operator_process_authority_id.is_none()
                && self.ordinary_task_authority_id.is_none()
                && self.ordinary_process_authority_id.is_none()
                && self.child_scope.is_some()
                && self.surface == TurnSurfaceKind::Worker,
            "ordinary task source was already bound or has no child scope"
        );
        self.ordinary_task_authority_id = Some(authority_id);
        Ok(())
    }

    pub(in crate::runtime) fn from_ordinary_process(
        authority: &super::child_source::ordinary_process::OrdinaryProcessAuthority,
    ) -> anyhow::Result<Self> {
        authority.verify()?;
        let snapshots = authority.snapshots();
        let parent = &snapshots.parent;
        let selection = authority.model_selection();
        let generation = selection.generation;
        let proof = Self {
            owner_isolated: parent.owner_isolated(),
            approval_review_adapter: None,
            previous_segment_input: None,
            segment_input_sequence: None,
            operator_task_authority_id: None,
            operator_process_authority_id: None,
            ordinary_task_authority_id: None,
            ordinary_process_authority_id: Some(authority.id()),
            tool_calls_allowed: true,
            intercepts_writes: false,
            has_conversation_history: false,
            child_scope: Some(snapshots.scope.clone()),
            workspace_id: parent.workspace_id().to_owned(),
            owner_id: parent.owner_id().to_owned(),
            session_key: authority.session_key().to_owned(),
            run_id: authority.run_id().to_owned(),
            task_id: Some(authority.run_id().to_owned()),
            routing_input: authority.task().to_owned(),
            provider_name: selection.provider_name.to_owned(),
            model_name: selection.model_name.to_owned(),
            config_source_revision: generation.source_revision.fingerprint_sha256.clone(),
            source_event_id: authority.input().map(|input| input.event().event_id.clone()),
            memory_scope: parent.memory_scope().clone(),
            source_memory_principal: child_memory_principal(
                parent.workspace_id(),
                &authority.logical_scope(),
                parent.owner_isolated() || parent.memory_scope().require_owner_match,
            )?,
            model_binding: AgentModelBinding::from_resolved(selection.resolved, selection.route),
            config_generation_id: generation.id.0,
            surface: TurnSurfaceKind::Worker,
            policy_channel: parent.policy_channel().to_owned(),
            policy_sender: parent.policy_sender().to_owned(),
            policy_chat_type: parent.policy_chat_type().to_owned(),
        };
        authority.verify()?;
        Ok(proof)
    }

    pub(in crate::runtime) fn from_operator_process(
        authority: &super::child_source::operator_process::OperatorProcessAuthority,
    ) -> anyhow::Result<Self> {
        authority.verify()?;
        let snapshots = authority.snapshots();
        let parent = &snapshots.parent;
        let selection = authority.model_selection();
        let generation = selection.generation;
        let proof = Self {
            owner_isolated: parent.owner_isolated(),
            approval_review_adapter: None,
            previous_segment_input: None,
            segment_input_sequence: None,
            operator_task_authority_id: None,
            operator_process_authority_id: Some(authority.id()),
            ordinary_task_authority_id: None,
            ordinary_process_authority_id: None,
            tool_calls_allowed: true,
            intercepts_writes: false,
            has_conversation_history: false,
            child_scope: None,
            workspace_id: parent.workspace_id().to_owned(),
            owner_id: parent.owner_id().to_owned(),
            session_key: authority.session_key().to_owned(),
            run_id: authority.run_id().to_owned(),
            task_id: Some(authority.run_id().to_owned()),
            routing_input: authority.task().to_owned(),
            provider_name: selection.provider_name.to_owned(),
            model_name: selection.model_name.to_owned(),
            config_source_revision: generation.source_revision.fingerprint_sha256.clone(),
            source_event_id: authority.input().map(|input| input.event().event_id.clone()),
            memory_scope: parent.memory_scope().clone(),
            source_memory_principal: child_memory_principal(
                parent.workspace_id(),
                &authority.logical_scope(),
                parent.owner_isolated() || parent.memory_scope().require_owner_match,
            )?,
            model_binding: AgentModelBinding::from_resolved(selection.resolved, selection.route),
            config_generation_id: generation.id.0,
            surface: TurnSurfaceKind::Worker,
            policy_channel: parent.policy_channel().to_owned(),
            policy_sender: parent.policy_sender().to_owned(),
            policy_chat_type: parent.policy_chat_type().to_owned(),
        };
        authority.verify()?;
        Ok(proof)
    }

    pub(crate) fn from_child_receipt(
        grant: &crate::runtime::child_source::IssuedChildGrant,
        receipt: &crate::runtime::child_source::ChildInputReceipt,
        generation: &crate::config::ConfigGeneration,
        selection: crate::runtime::child_source::ChildModelSelection<'_>,
    ) -> anyhow::Result<Self> {
        use anyhow::Context;
        anyhow::ensure!(
            grant == selection.grant,
            "child provider selection belongs to another grant"
        );
        grant.verify_receipt(receipt)?;
        let event = receipt.event();
        let parent = grant.parent();
        anyhow::ensure!(
            generation.id.0 == parent.config_generation_id()
                && generation.source_revision.fingerprint_sha256 == parent.config_source_revision()
                && generation.effective.workspace_dir.to_string_lossy() == event.workspace_id
                && ingress_event_surface(&event.source, event.channel.as_deref(), &event.visibility)
                    == Some(TurnSurfaceKind::Worker)
                && !selection.provider_name.is_empty()
                && !selection.model_name.is_empty(),
            "child model generation differs from admitted source"
        );
        let tool_calls_allowed = if event.source == crate::memory::MessageEventSource::Delegate {
            let agent = event.agent_id.as_deref().context("delegate source agent missing")?;
            generation
                .effective
                .agents
                .get(agent)
                .context("delegate source agent is not configured")?
                .agentic
        } else {
            true
        };
        Ok(Self {
            owner_isolated: parent.owner_isolated(),
            approval_review_adapter: None,
            previous_segment_input: receipt.previous_segment_input().map(str::to_owned),
            segment_input_sequence: Some(event.id),
            operator_task_authority_id: None,
            operator_process_authority_id: None,
            ordinary_task_authority_id: None,
            ordinary_process_authority_id: None,
            tool_calls_allowed,
            intercepts_writes: false,
            has_conversation_history: false,
            child_scope: Some(grant.scope().clone()),
            workspace_id: event.workspace_id.clone(),
            owner_id: parent.owner_id().to_owned(),
            session_key: event.session_key.clone().context("child session missing")?,
            run_id: event.run_id.clone().context("child run missing")?,
            task_id: subject_task_id(event.subject.as_ref()).map(str::to_owned),
            routing_input: receipt.input().to_owned(),
            provider_name: selection.provider_name.to_owned(),
            model_name: selection.model_name.to_owned(),
            config_source_revision: generation.source_revision.fingerprint_sha256.clone(),
            source_event_id: Some(event.event_id.clone()),
            // Recall authority is inherited from the admitted parent; a new
            // child session does not itself authorize broader memory reads.
            memory_scope: parent.memory_scope().clone(),
            source_memory_principal: crate::memory::MemoryPrincipal {
                workspace_id: event.workspace_id.clone(),
                owner_id: Some(parent.owner_id().to_owned()),
                session_key: event.session_key.clone(),
                agent_id: event.agent_id.clone(),
                persona_id: event.persona_id.clone(),
                channel: event.channel.clone(),
                sender: event.sender.clone(),
                require_owner_match: parent.owner_isolated() || parent.memory_scope().require_owner_match,
                legacy_session_key: None,
            },
            model_binding: AgentModelBinding::from_resolved(selection.resolved, selection.route),
            config_generation_id: generation.id.0,
            surface: TurnSurfaceKind::Worker,
            policy_channel: parent.policy_channel().to_owned(),
            policy_sender: parent.policy_sender().to_owned(),
            policy_chat_type: parent.policy_chat_type().to_owned(),
        })
    }

    /// Disabled ordinary source provenance, without a fabricated parent grant
    /// or durable event. Service admission remains a separate boundary.
    pub(super) fn from_ordinary_child(
        caller: &crate::tools::execution::ChildCallContext,
        scope: &crate::memory::MessageEventScope,
        input: &str,
        receipt: Option<&super::child_source::ChildInputReceipt>,
        selection: super::child_source::OrdinaryChildModelSelection<'_>,
    ) -> anyhow::Result<Self> {
        use anyhow::Context;
        let delegate = match scope.source {
            crate::memory::MessageEventSource::Delegate => {
                caller.verify_tool("delegate")?;
                true
            }
            crate::memory::MessageEventSource::SessionsSpawn => {
                caller.verify_tool("sessions_spawn")?;
                false
            }
            _ => anyhow::bail!("unsupported ordinary child source"),
        };
        let generation = selection.generation;
        anyhow::ensure!(
            !generation.effective.causal_tree.enabled,
            "ordinary source requires disabled plane"
        );
        let actor = caller.context();
        let tool_calls_allowed = if delegate {
            let agent = scope.agent_id.as_deref().context("ordinary delegate agent missing")?;
            generation
                .effective
                .agents
                .get(agent)
                .context("ordinary delegate agent is not configured")?
                .agentic
        } else {
            true
        };
        let owner_id = scope.owner_id.clone().context("ordinary child owner missing")?;
        let memory_scope = match &actor.ordinary_child_publication {
            Some(publication) => {
                publication.verify_caller(actor)?;
                publication.memory_scope().clone()
            }
            None => {
                let mut scope = actor.envelope.memory_write_context(actor.chat_type.clone());
                scope.owner_id = Some(owner_id.clone());
                scope.require_owner_match = actor.envelope.owner_isolated;
                scope
            }
        };
        Ok(Self {
            owner_isolated: actor.envelope.owner_isolated,
            approval_review_adapter: None,
            previous_segment_input: None,
            segment_input_sequence: None,
            operator_task_authority_id: None,
            operator_process_authority_id: None,
            ordinary_task_authority_id: None,
            ordinary_process_authority_id: None,
            tool_calls_allowed,
            intercepts_writes: actor
                .ordinary_child_publication
                .as_ref()
                .is_some_and(|publication| publication.intercepts_writes()),
            has_conversation_history: false,
            child_scope: actor
                .ordinary_child_publication
                .as_ref()
                .map(|publication| publication.scope().clone()),
            workspace_id: actor.envelope.workspace_id.clone(),
            owner_id,
            session_key: scope.session_key.clone().context("ordinary child session missing")?,
            run_id: scope.run_id.clone().context("ordinary child run missing")?,
            task_id: subject_task_id(scope.subject.as_ref()).map(str::to_owned),
            routing_input: input.to_owned(),
            provider_name: selection.provider_name.to_owned(),
            model_name: selection.model_name.to_owned(),
            config_source_revision: generation.source_revision.fingerprint_sha256.clone(),
            source_event_id: receipt.map(|receipt| receipt.event().event_id.clone()),
            source_memory_principal: child_memory_principal(
                &actor.envelope.workspace_id,
                scope,
                actor.envelope.owner_isolated || memory_scope.require_owner_match,
            )?,
            memory_scope,
            model_binding: AgentModelBinding::from_resolved(selection.resolved, selection.route),
            config_generation_id: generation.id.0,
            surface: TurnSurfaceKind::Worker,
            policy_channel: scope.channel.clone().context("ordinary child channel missing")?,
            policy_sender: scope.sender.clone().context("ordinary child sender missing")?,
            policy_chat_type: actor.chat_type.clone(),
        })
    }

    pub(in crate::runtime) fn ordinary_process_segment(
        &self,
        authority: &super::child_source::ordinary_process::OrdinaryProcessAuthority,
        run_id: &str,
        input: &str,
        receipt: Option<&super::child_source::ChildInputReceipt>,
    ) -> anyhow::Result<Self> {
        authority.verify_source(self)?;
        anyhow::ensure!(
            !run_id.is_empty() && run_id != authority.run_id(),
            "ordinary worker segment identity missing"
        );
        if let Some(receipt) = receipt {
            let event = receipt.event();
            anyhow::ensure!(
                event.workspace_id == self.workspace_id
                    && event.owner_id.as_deref() == Some(self.owner_id.as_str())
                    && event.session_key.as_deref() == Some(self.session_key.as_str())
                    && event.run_id.as_deref() == Some(run_id)
                    && event.source == crate::memory::MessageEventSource::SessionWorker
                    && receipt.input() == input,
                "ordinary worker segment receipt differs from source"
            );
        }
        let mut proof = Self::capture(self);
        proof.run_id = run_id.to_owned();
        proof.task_id = Some(authority.run_id().to_owned());
        proof.routing_input = input.to_owned();
        proof.source_event_id = receipt.map(|receipt| receipt.event().event_id.clone());
        proof.previous_segment_input = receipt
            .and_then(|receipt| receipt.previous_segment_input())
            .map(str::to_owned);
        proof.segment_input_sequence = receipt.map(|receipt| receipt.event().id);
        Ok(proof)
    }

    pub(in crate::runtime) fn operator_process_segment(
        &self,
        authority: &super::child_source::operator_process::OperatorProcessAuthority,
        run_id: &str,
        input: &str,
        receipt: Option<&super::child_source::ChildInputReceipt>,
    ) -> anyhow::Result<Self> {
        authority.verify_source(self)?;
        anyhow::ensure!(
            !run_id.is_empty() && run_id != authority.run_id(),
            "operator worker segment identity missing"
        );
        if let Some(receipt) = receipt {
            receipt.verify_backend(&authority.source_fabric().memory())?;
            let event = receipt.event();
            anyhow::ensure!(
                event.id > 0
                    && event.workspace_id == self.workspace_id
                    && event.owner_id.as_deref() == Some(self.owner_id.as_str())
                    && event.session_key.as_deref() == Some(self.session_key.as_str())
                    && event.run_id.as_deref() == Some(run_id)
                    && event.source == crate::memory::MessageEventSource::SessionWorker
                    && subject_task_id(event.subject.as_ref()) == Some(authority.run_id())
                    && event.config_generation_id == Some(self.config_generation_id)
                    && event.config_source_revision.as_deref() == Some(self.config_source_revision.as_str())
                    && receipt.input() == input,
                "operator worker segment receipt differs from source"
            );
        } else {
            let recording = authority
                .snapshots()
                .generation
                .effective
                .memory
                .event_recording_config();
            anyhow::ensure!(
                !recording.enabled || !recording.record_user_messages,
                "operator worker segment receipt missing"
            );
        }
        let mut proof = Self::capture(self);
        proof.run_id = run_id.to_owned();
        proof.task_id = Some(authority.run_id().to_owned());
        proof.routing_input = input.to_owned();
        proof.source_event_id = receipt.map(|receipt| receipt.event().event_id.clone());
        proof.previous_segment_input = receipt
            .and_then(|receipt| receipt.previous_segment_input())
            .map(str::to_owned);
        proof.segment_input_sequence = receipt.map(|receipt| receipt.event().id);
        authority.verify_source(&proof)?;
        Ok(proof)
    }

    pub(super) fn ordinary_task_segment(
        &self,
        authority: &super::child_source::ordinary_task::OrdinaryTaskAuthority,
        run_id: &str,
        input: &str,
        receipt: Option<&super::child_source::ChildInputReceipt>,
    ) -> anyhow::Result<Self> {
        authority.verify_source(self)?;
        anyhow::ensure!(
            !run_id.is_empty() && run_id != self.run_id,
            "ordinary segment run identity missing"
        );
        if let Some(receipt) = receipt {
            let event = receipt.event();
            anyhow::ensure!(
                event.workspace_id == self.workspace_id
                    && event.owner_id.as_deref() == Some(self.owner_id.as_str())
                    && event.session_key.as_deref() == Some(self.session_key.as_str())
                    && event.run_id.as_deref() == Some(run_id)
                    && receipt.input() == input,
                "ordinary segment receipt differs from issued source"
            );
        }
        let mut proof = Self::capture(self);
        proof.run_id = run_id.to_owned();
        proof.task_id = Some(self.run_id.clone());
        proof.routing_input = input.to_owned();
        proof.source_event_id = receipt.map(|receipt| receipt.event().event_id.clone());
        proof.previous_segment_input = receipt
            .and_then(|receipt| receipt.previous_segment_input())
            .map(str::to_owned);
        proof.segment_input_sequence = receipt.map(|receipt| receipt.event().id);
        Ok(proof)
    }

    pub(in crate::runtime) fn operator_task_segment(
        authority: &super::operator_spawn::task::OperatorTaskAuthority,
        run_id: &str,
        input: &str,
        receipt: Option<&super::child_source::ChildInputReceipt>,
    ) -> anyhow::Result<Self> {
        authority.verify_source(authority)?;
        anyhow::ensure!(
            !run_id.is_empty() && run_id != authority.run_id(),
            "operator segment run identity missing"
        );
        let recording = authority.generation().effective.memory.event_recording_config();
        if let Some(receipt) = receipt {
            receipt.verify_backend(&authority.fabric().memory())?;
            let event = receipt.event();
            anyhow::ensure!(
                event.id > 0
                    && event.workspace_id == authority.workspace_id()
                    && event.owner_id.as_deref() == Some(authority.owner_id())
                    && event.session_key.as_deref() == Some(authority.session_key())
                    && event.run_id.as_deref() == Some(run_id)
                    && subject_task_id(event.subject.as_ref()) == Some(authority.run_id())
                    && event.source == crate::memory::MessageEventSource::SessionsSpawn
                    && event.config_generation_id == Some(authority.config_generation_id())
                    && event.config_source_revision.as_deref() == Some(authority.config_source_revision())
                    && receipt.input() == input,
                "operator segment receipt differs from issued source"
            );
        } else {
            anyhow::ensure!(
                !recording.enabled || !recording.record_user_messages,
                "operator segment requires an input receipt"
            );
        }
        let mut proof = Self::capture(authority);
        proof.run_id = run_id.to_owned();
        proof.routing_input = input.to_owned();
        proof.source_event_id = receipt.map(|receipt| receipt.event().event_id.clone());
        proof.previous_segment_input = receipt
            .and_then(|receipt| receipt.previous_segment_input())
            .map(str::to_owned);
        proof.segment_input_sequence = receipt.map(|receipt| receipt.event().id);
        authority.verify_source(&proof)?;
        Ok(proof)
    }

    pub(crate) fn capture(proof: &dyn TurnSourceProof) -> Self {
        Self {
            owner_isolated: proof.owner_isolated(),
            approval_review_adapter: proof.approval_review_adapter(),
            previous_segment_input: None,
            segment_input_sequence: None,
            operator_task_authority_id: proof.operator_task_authority_id(),
            operator_process_authority_id: proof.operator_process_authority_id(),
            ordinary_task_authority_id: proof.ordinary_task_authority_id(),
            ordinary_process_authority_id: proof.ordinary_process_authority_id(),
            tool_calls_allowed: proof.tool_calls_allowed(),
            intercepts_writes: proof.intercepts_writes(),
            has_conversation_history: proof.has_conversation_history(),
            child_scope: proof.child_scope().cloned(),
            workspace_id: proof.workspace_id().to_owned(),
            owner_id: proof.owner_id().to_owned(),
            session_key: proof.session_key().to_owned(),
            run_id: proof.run_id().to_owned(),
            task_id: proof.task_id().map(str::to_owned),
            routing_input: proof.routing_input().to_owned(),
            provider_name: proof.provider_name().to_owned(),
            model_name: proof.model_name().to_owned(),
            config_source_revision: proof.config_source_revision().to_owned(),
            source_event_id: proof.source_event_id().map(str::to_owned),
            memory_scope: proof.memory_scope().clone(),
            source_memory_principal: proof.source_memory_principal(),
            model_binding: proof.model_binding().clone(),
            config_generation_id: proof.config_generation_id(),
            surface: proof.surface(),
            policy_channel: proof.policy_channel().to_owned(),
            policy_sender: proof.policy_sender().to_owned(),
            policy_chat_type: proof.policy_chat_type().to_owned(),
        }
    }
}

impl TurnSourceProof for OwnedTurnSourceProof {
    fn owner_isolated(&self) -> bool {
        self.owner_isolated
    }
    fn approval_review_adapter(&self) -> Option<crate::causal_tree::approval_collection::ApprovalReviewAdapter> {
        self.approval_review_adapter
    }
    fn source_memory_principal(&self) -> crate::memory::MemoryPrincipal {
        self.source_memory_principal.clone()
    }

    fn operator_task_authority_id(&self) -> Option<uuid::Uuid> {
        self.operator_task_authority_id
    }
    fn operator_process_authority_id(&self) -> Option<uuid::Uuid> {
        self.operator_process_authority_id
    }
    fn ordinary_process_authority_id(&self) -> Option<uuid::Uuid> {
        self.ordinary_process_authority_id
    }
    fn ordinary_task_authority_id(&self) -> Option<uuid::Uuid> {
        self.ordinary_task_authority_id
    }
    fn tool_calls_allowed(&self) -> bool {
        self.tool_calls_allowed
    }
    fn intercepts_writes(&self) -> bool {
        self.intercepts_writes
    }
    fn has_conversation_history(&self) -> bool {
        self.has_conversation_history
    }
    fn child_scope(&self) -> Option<&std::sync::Arc<crate::tools::capability::child::VerifiedChildScope>> {
        self.child_scope.as_ref()
    }
    fn workspace_id(&self) -> &str {
        &self.workspace_id
    }
    fn owner_id(&self) -> &str {
        &self.owner_id
    }
    fn session_key(&self) -> &str {
        &self.session_key
    }
    fn run_id(&self) -> &str {
        &self.run_id
    }
    fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }
    fn routing_input(&self) -> &str {
        &self.routing_input
    }
    fn provider_name(&self) -> &str {
        &self.provider_name
    }
    fn model_name(&self) -> &str {
        &self.model_name
    }
    fn config_source_revision(&self) -> &str {
        &self.config_source_revision
    }
    fn source_event_id(&self) -> Option<&str> {
        self.source_event_id.as_deref()
    }
    fn memory_scope(&self) -> &MemoryWriteContext {
        &self.memory_scope
    }
    fn model_binding(&self) -> &AgentModelBinding {
        &self.model_binding
    }
    fn config_generation_id(&self) -> u64 {
        self.config_generation_id
    }
    fn surface(&self) -> TurnSurfaceKind {
        self.surface
    }
    fn policy_channel(&self) -> &str {
        &self.policy_channel
    }
    fn policy_sender(&self) -> &str {
        &self.policy_sender
    }
    fn policy_chat_type(&self) -> &str {
        &self.policy_chat_type
    }
}

#[cfg(test)]
mod child_retrieval_principal_tests {
    use super::*;

    #[test]
    fn child_read_identity_uses_child_session_and_preserves_isolation() {
        let mut scope =
            crate::memory::MessageEventScope::new("session_worker", crate::memory::MemoryVisibility::Session)
                .with_owner_id("prx_owner")
                .with_session_key("prx_child_session");
        scope.parent_session_key = Some("prx_parent_session".to_owned());
        scope.agent_id = Some("prx_agent".to_owned());
        scope.persona_id = Some("prx_persona".to_owned());
        scope.channel = Some("prx_channel".to_owned());
        scope.sender = Some("prx_user".to_owned());
        for isolated in [false, true] {
            let principal = child_memory_principal("prx_workspace", &scope, isolated).unwrap();
            assert_eq!(principal.workspace_id, "prx_workspace");
            assert_eq!(principal.owner_id.as_deref(), Some("prx_owner"));
            assert_eq!(principal.session_key.as_deref(), Some("prx_child_session"));
            assert_eq!(principal.agent_id, scope.agent_id);
            assert_eq!(principal.persona_id, scope.persona_id);
            assert_eq!(principal.channel, scope.channel);
            assert_eq!(principal.sender, scope.sender);
            assert_eq!(principal.require_owner_match, isolated);
            assert!(principal.legacy_session_key.is_none());
        }
        let mut missing = scope.clone();
        missing.owner_id = Some(" ".to_owned());
        assert!(child_memory_principal("prx_workspace", &missing, true).is_err());
        missing = scope;
        missing.session_key = None;
        assert!(child_memory_principal("prx_workspace", &missing, true).is_err());
    }
}
