//! Independent child lifetimes issued by operator or adopted-message ingress.
mod mailbox;
pub(crate) mod process;
pub(crate) mod task;

use crate::runtime::turn_source::{OwnedTurnSourceProof, TurnSourceProof};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio_util::sync::CancellationToken;

pub(crate) struct ChildLaunchSource {
    live_turn: Option<crate::agent::live_frames::LiveTurnRef>,
    message_event: Option<String>,
    message_permit: Option<Arc<crate::runtime::agent_admission::AgentTurnPermit>>,
    message_input: Option<crate::messaging::runtime::MessageInput>,
    continuation: Option<crate::memory::child_sessions::ChildSessionRecord>,
    proof: OwnedTurnSourceProof,
    generation: Arc<crate::config::ConfigGeneration>,
    fabric: crate::memory::MemoryFabric,
    provider: Arc<dyn crate::providers::Provider>,
    catalog: crate::providers::catalog::CatalogView,
    resolved: crate::providers::catalog::ResolvedModel,
    route: Option<crate::providers::catalog::RouteTarget>,
    security: Arc<crate::security::SecurityPolicy>,
    publication: crate::runtime::child_source::publication::IndependentChildPublication,
    task: String,
    claimed: AtomicBool,
    child_issued: AtomicBool,
    cancellation: CancellationToken,
}

struct OperatorLifetime(CancellationToken);
impl Drop for OperatorLifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

tokio::task_local! { static OPERATOR_SPAWN: Arc<ChildLaunchSource>; }

pub(crate) fn current() -> Option<Arc<ChildLaunchSource>> {
    OPERATOR_SPAWN.try_with(Arc::clone).ok()
}

impl ChildLaunchSource {
    pub(crate) fn new(
        proof: &dyn TurnSourceProof,
        generation: Arc<crate::config::ConfigGeneration>,
        fabric: crate::memory::MemoryFabric,
        provider: Arc<dyn crate::providers::Provider>,
        task: String,
        security: Arc<crate::security::SecurityPolicy>,
        publication: Arc<crate::runtime::turn_publication::TurnRuntimeService>,
        runtime: Arc<crate::runtime::turn_context::TurnRuntimeHolder>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            proof.surface() == crate::tools::capability::turn_state::TurnSurfaceKind::Interactive
                && proof.policy_channel() == "terminal"
                && proof.policy_sender() == "local-user"
                && proof.config_generation_id() == generation.id.0
                && proof.config_source_revision() == generation.source_revision.fingerprint_sha256
                && proof.workspace_id() == fabric.workspace_id()
                && proof.workspace_id() == generation.effective.workspace_dir.to_string_lossy()
                && !proof.owner_id().is_empty()
                && !task.is_empty(),
            "operator source identity differs"
        );
        let publication = crate::runtime::child_source::publication::IndependentChildPublication::capture(
            publication,
            runtime,
            proof,
            &generation,
            &fabric.memory(),
            &security,
        )?;
        let catalog = crate::providers::selection::catalog_for(&generation.effective);
        let resolved = crate::providers::selection::check_with(
            &generation.effective,
            &catalog,
            proof.provider_name(),
            proof.model_name(),
            proof.model_binding().credential_reference().clone(),
            &crate::providers::catalog::Requirement::chat(),
            crate::providers::catalog::UnknownPolicy::Invocation,
        )?;
        let route =
            crate::providers::selection::route_target(&catalog, proof.provider_name(), proof.model_name())?.cloned();
        anyhow::ensure!(
            proof.model_binding().matches(&resolved) && proof.model_binding().route() == route.as_ref(),
            "operator model source differs from pinned PMM selection"
        );
        Ok(Self {
            live_turn: None,
            message_event: None,
            message_permit: None,
            message_input: None,
            continuation: None,
            catalog,
            resolved,
            route,
            proof: OwnedTurnSourceProof::capture(proof),
            generation,
            fabric,
            provider,
            task,
            security,
            publication,
            claimed: AtomicBool::new(false),
            child_issued: AtomicBool::new(false),
            cancellation: CancellationToken::new(),
        })
    }

    pub(crate) fn for_continuation(
        mut self,
        record: crate::memory::child_sessions::ChildSessionRecord,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            record.owner == self.proof.owner_id()
                && record.workspace == self.proof.workspace_id()
                && record.parent_session == self.proof.session_key()
                && record.provider == self.proof.provider_name()
                && record.model == self.proof.model_name(),
            "operator continuation identity differs"
        );
        self.continuation = Some(record);
        Ok(self)
    }

    /// A mailbox wake authorizes exactly one existing receiver session. It
    /// never authorizes a new spawn or an operator command in the message body.
    pub(crate) async fn for_message(
        mut self,
        record: crate::memory::child_sessions::ChildSessionRecord,
        input: &crate::messaging::runtime::MessageInput,
    ) -> anyhow::Result<Self> {
        let runtime = crate::messaging::runtime::MessagingRuntime::for_config(&self.generation.effective)?;
        input.verify(&runtime)?;
        let id = input
            .receipt
            .adopted_message_event_id
            .ok_or_else(|| anyhow::anyhow!("mailbox adoption missing"))?
            .to_string();
        let event = self
            .fabric
            .memory()
            .find_owned_message_event(&record.workspace, &record.owner, &id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("mailbox input is no longer live"))?;
        anyhow::ensure!(
            input.message.to.session == record.session_key
                && input.message.to.owner == record.owner
                && event.session_key.as_deref() == Some(record.session_key.as_str())
                && event.role == "user"
                && event.source.as_str() == "prx_mailbox"
                && event.content == input.text
                && input.auto_start(),
            "mailbox wake identity differs"
        );
        self = self.for_continuation(record)?;
        let runtime = crate::messaging::runtime::MessagingRuntime::for_config(&self.generation.effective)?;
        let scope = runtime.resolve_scope(&input.message.to)?;
        let root = input.message.to.agent.clone();
        runtime.store.validate_scope(&scope, &input.message.to)?;
        self.message_permit =
            crate::runtime::agent_admission::admit(&self.generation.effective, self.proof.owner_id(), &root)
                .map_err(|error| match error {
                    crate::runtime::agent_admission::AdmissionError::Busy => {
                        anyhow::Error::new(crate::runtime::mailbox_child::ChildBusy)
                    }
                    error => anyhow::Error::new(error),
                })?
                .map(Arc::new);
        self.message_event = Some(id);
        self.message_input = Some(input.clone());
        Ok(self)
    }

    pub(crate) const fn is_message_continuation(&self) -> bool {
        self.message_event.is_some() && self.continuation.is_some()
    }
    pub(crate) fn message_input(&self) -> Option<crate::messaging::runtime::MessageInput> {
        self.message_input.clone()
    }
    pub(crate) fn message_permit(&self) -> Option<Arc<crate::runtime::agent_admission::AgentTurnPermit>> {
        self.message_permit.clone()
    }
    pub(crate) fn live_turn(&self) -> Option<crate::agent::live_frames::LiveTurnRef> {
        self.live_turn.clone()
    }

    pub(crate) async fn scope<F: std::future::Future>(self, work: F) -> F::Output {
        let _lifetime = OperatorLifetime(self.cancellation.clone());
        OPERATOR_SPAWN.scope(Arc::new(self), work).await
    }

    pub(crate) fn verify_live(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.cancellation.is_cancelled(), "operator spawn source is closed");
        self.publication
            .verify(&self.proof, &self.generation, &self.fabric.memory(), &self.security)
    }

    pub(crate) const fn catalog(&self) -> &crate::providers::catalog::CatalogView {
        &self.catalog
    }
    pub(crate) const fn resolved(&self) -> &crate::providers::catalog::ResolvedModel {
        &self.resolved
    }
    pub(crate) const fn route(&self) -> Option<&crate::providers::catalog::RouteTarget> {
        self.route.as_ref()
    }

    pub(crate) const fn security(&self) -> &Arc<crate::security::SecurityPolicy> {
        &self.security
    }

    pub(crate) fn bind_input_scope(&self, scope: &mut crate::memory::MessageEventScope) -> anyhow::Result<()> {
        self.verify_live()?;
        anyhow::ensure!(
            scope.source == crate::memory::MessageEventSource::SessionsSpawn
                && scope
                    .run_id
                    .as_deref()
                    .is_some_and(|run| !run.is_empty() && run != self.proof.run_id()),
            "operator child input requires a distinct spawn run"
        );
        scope.subject = scope.run_id.clone().map(crate::memory::MessageEventSubject::Task);
        scope.owner_id = Some(self.proof.owner_id().to_owned());
        scope.parent_session_key = Some(self.proof.session_key().to_owned());
        scope.parent_run_id = Some(self.proof.run_id().to_owned());
        scope.channel = Some(self.proof.policy_channel().to_owned());
        scope.sender = Some(self.proof.policy_sender().to_owned());
        scope.recipient = self.proof.memory_scope().chat_id.clone();
        scope.agent_partition =
            crate::runtime::child_source::inherited_partition(self.proof.memory_scope().agent_partition.as_ref());
        scope.causation_event_id = self
            .message_event
            .clone()
            .or_else(|| self.proof.source_event_id().map(str::to_owned));
        scope.config_generation_id = Some(self.generation.id.0);
        scope.config_source_revision = Some(self.generation.source_revision.fingerprint_sha256.clone());
        Ok(())
    }

    pub(crate) const fn proof(&self) -> &OwnedTurnSourceProof {
        &self.proof
    }
    pub(crate) const fn generation(&self) -> &Arc<crate::config::ConfigGeneration> {
        &self.generation
    }
    pub(crate) const fn fabric(&self) -> &crate::memory::MemoryFabric {
        &self.fabric
    }
    pub(crate) fn provider(&self) -> &Arc<dyn crate::providers::Provider> {
        &self.provider
    }
}

pub(crate) fn claim(args: &serde_json::Value) -> anyhow::Result<Option<Arc<ChildLaunchSource>>> {
    let Some(source) = OPERATOR_SPAWN.try_with(Arc::clone).ok() else {
        return Ok(None);
    };
    source.verify_live()?;
    anyhow::ensure!(
        args.as_object().is_some_and(|object| object.keys().all(|key| matches!(
            key.as_str(),
            "task" | "provider" | "model" | "action" | "session_key" | "mode" | "agent"
        ) || key
            == crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG)),
        "operator spawn contains uncaptured overrides"
    );
    anyhow::ensure!(
        args.get("task").and_then(serde_json::Value::as_str) == Some(source.task.as_str())
            && args.get("provider").and_then(serde_json::Value::as_str) == Some(source.proof.provider_name())
            && args.get("model").and_then(serde_json::Value::as_str) == Some(source.proof.model_name())
            && args
                .get("action")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|action| action
                    == if source.continuation.is_some() {
                        "continue"
                    } else {
                        "spawn"
                    })
            && source.continuation.as_ref().map_or_else(
                || !["session_key", "mode", "agent"]
                    .iter()
                    .any(|key| args.get(key).is_some()),
                |record| args.get("session_key").and_then(serde_json::Value::as_str)
                    == Some(record.session_key.as_str())
                    && args.get("mode").and_then(serde_json::Value::as_str) == Some(record.mode.as_str())
                    && args.get("agent").and_then(serde_json::Value::as_str).unwrap_or("main") == record.agent
            ),
        "operator spawn differs from captured command"
    );
    anyhow::ensure!(
        source
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok(),
        "operator spawn command was already consumed"
    );
    source.verify_live()?;
    Ok(Some(source))
}
