//! Typed local-operator ingress, separate from model delegation authority.
pub(crate) mod process;
pub(crate) mod task;

use crate::runtime::turn_source::{OwnedTurnSourceProof, TurnSourceProof};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio_util::sync::CancellationToken;

pub(crate) struct OperatorSpawnSource {
    proof: OwnedTurnSourceProof,
    generation: Arc<crate::config::ConfigGeneration>,
    fabric: crate::memory::MemoryFabric,
    provider: Arc<dyn crate::providers::Provider>,
    catalog: crate::providers::catalog::CatalogView,
    resolved: crate::providers::catalog::ResolvedModel,
    route: Option<crate::providers::catalog::RouteTarget>,
    security: Arc<crate::security::SecurityPolicy>,
    publication: crate::runtime::turn_decision::OperatorChildPublication,
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

tokio::task_local! { static OPERATOR_SPAWN: Arc<OperatorSpawnSource>; }

impl OperatorSpawnSource {
    pub(crate) fn new(
        proof: &dyn TurnSourceProof,
        generation: Arc<crate::config::ConfigGeneration>,
        fabric: crate::memory::MemoryFabric,
        provider: Arc<dyn crate::providers::Provider>,
        task: String,
        security: Arc<crate::security::SecurityPolicy>,
        publication: Arc<crate::runtime::turn_decision::TurnDecisionService>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            proof.surface() == crate::causal_tree::state::TurnSurfaceKind::Interactive
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
        let publication = crate::runtime::turn_decision::OperatorChildPublication::capture(
            publication,
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
        scope.session_key = Some(self.proof.session_key().to_owned());
        scope.parent_session_key = scope.session_key.clone();
        scope.parent_run_id = Some(self.proof.run_id().to_owned());
        scope.channel = Some(self.proof.policy_channel().to_owned());
        scope.sender = Some(self.proof.policy_sender().to_owned());
        scope.recipient = self.proof.memory_scope().chat_id.clone();
        scope.causation_event_id = self.proof.source_event_id().map(str::to_owned);
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

pub(crate) fn claim(args: &serde_json::Value) -> anyhow::Result<Option<Arc<OperatorSpawnSource>>> {
    let Some(source) = OPERATOR_SPAWN.try_with(Arc::clone).ok() else {
        return Ok(None);
    };
    source.verify_live()?;
    anyhow::ensure!(
        args.as_object().is_some_and(|object| object.keys().all(|key| matches!(
            key.as_str(),
            "task" | "provider" | "model" | "action"
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
                .is_none_or(|action| action == "spawn"),
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
