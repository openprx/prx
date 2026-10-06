//! Parent provenance for child issuance, deliberately not a TurnSourceProof.
use super::{Arc, ChildInputReceipt, Context, MemoryFabric};
use crate::causal_tree::state::TurnSurfaceKind;
use crate::decision::key::{DecisionKey, KeyDomain};
use crate::memory::principal::MemoryWriteContext;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub(crate) struct ParentSource {
    owner_isolated: bool,
    workspace_id: String,
    owner_id: String,
    session_key: String,
    run_id: String,
    routing_input: String,
    config_source_revision: String,
    source_event_id: Option<String>,
    config_generation_id: u64,
    policy_channel: String,
    policy_sender: String,
    policy_chat_type: String,
    surface: TurnSurfaceKind,
    memory_scope: MemoryWriteContext,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentSourceSnapshot {
    payload: String,
    seal: [u8; 32],
}
impl ParentSourceSnapshot {
    pub(super) fn matches_binding(&self, other: &Self) -> bool {
        self.payload == other.payload && self.seal == other.seal
    }
}
impl std::fmt::Debug for ParentSourceSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParentSourceSnapshot").finish_non_exhaustive()
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecallScope {
    workspace_id: Option<String>,
    channel: Option<String>,
    chat_type: Option<String>,
    chat_id: Option<String>,
    sender_id: Option<String>,
    raw_sender: Option<String>,
    owner_id: Option<String>,
    require_owner_match: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceFields {
    // Required signed provenance: old payloads must not default to unisolated.
    owner_isolated: bool,
    workspace_id: String,
    owner_id: String,
    session_key: String,
    run_id: String,
    routing_input: String,
    config_source_revision: String,
    source_event_id: Option<String>,
    config_generation_id: u64,
    policy_channel: String,
    policy_sender: String,
    policy_chat_type: String,
    surface: String,
    memory_scope: RecallScope,
}
impl ParentSource {
    pub(crate) fn capture(proof: &dyn crate::runtime::turn_source::TurnSourceProof) -> Self {
        Self {
            owner_isolated: proof.owner_isolated(),
            workspace_id: proof.workspace_id().to_owned(),
            owner_id: proof.owner_id().to_owned(),
            session_key: proof.session_key().to_owned(),
            run_id: proof.run_id().to_owned(),
            routing_input: proof.routing_input().to_owned(),
            config_source_revision: proof.config_source_revision().to_owned(),
            source_event_id: proof.source_event_id().map(str::to_owned),
            config_generation_id: proof.config_generation_id(),
            policy_channel: proof.policy_channel().to_owned(),
            policy_sender: proof.policy_sender().to_owned(),
            policy_chat_type: proof.policy_chat_type().to_owned(),
            surface: proof.surface(),
            memory_scope: proof.memory_scope().clone(),
        }
    }
    pub(crate) const fn owner_isolated(&self) -> bool {
        self.owner_isolated
    }
    pub(crate) fn workspace_id(&self) -> &str {
        &self.workspace_id
    }
    pub(crate) fn owner_id(&self) -> &str {
        &self.owner_id
    }
    pub(crate) fn session_key(&self) -> &str {
        &self.session_key
    }
    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }
    pub(crate) fn routing_input(&self) -> &str {
        &self.routing_input
    }
    pub(crate) fn config_source_revision(&self) -> &str {
        &self.config_source_revision
    }
    pub(crate) fn source_event_id(&self) -> Option<&str> {
        self.source_event_id.as_deref()
    }
    pub(crate) const fn config_generation_id(&self) -> u64 {
        self.config_generation_id
    }
    pub(crate) fn policy_channel(&self) -> &str {
        &self.policy_channel
    }
    pub(crate) fn policy_sender(&self) -> &str {
        &self.policy_sender
    }
    pub(crate) fn policy_chat_type(&self) -> &str {
        &self.policy_chat_type
    }
    pub(crate) const fn surface(&self) -> TurnSurfaceKind {
        self.surface
    }
    pub(crate) const fn memory_scope(&self) -> &MemoryWriteContext {
        &self.memory_scope
    }

    pub(crate) fn bind_process(
        &self,
        decision: crate::causal_tree::ids::DecisionId,
        key: &DecisionKey,
    ) -> anyhow::Result<ParentSourceSnapshot> {
        let payload = self.process_payload()?;
        let seal = key.hmac(
            KeyDomain::StateFingerprint,
            &[
                b"worker-parent-source-v1",
                decision.to_string().as_bytes(),
                payload.as_bytes(),
            ],
        );
        Ok(ParentSourceSnapshot { payload, seal })
    }

    fn process_payload(&self) -> anyhow::Result<String> {
        let fields = SourceFields {
            owner_isolated: self.owner_isolated,
            workspace_id: self.workspace_id.clone(),
            owner_id: self.owner_id.clone(),
            session_key: self.session_key.clone(),
            run_id: self.run_id.clone(),
            routing_input: self.routing_input.clone(),
            config_source_revision: self.config_source_revision.clone(),
            source_event_id: self.source_event_id.clone(),
            config_generation_id: self.config_generation_id,
            policy_channel: self.policy_channel.clone(),
            policy_sender: self.policy_sender.clone(),
            policy_chat_type: self.policy_chat_type.clone(),
            surface: self.surface.as_str().to_owned(),
            memory_scope: RecallScope {
                workspace_id: self.memory_scope.workspace_id.clone(),
                channel: self.memory_scope.channel.clone(),
                chat_type: self.memory_scope.chat_type.clone(),
                chat_id: self.memory_scope.chat_id.clone(),
                sender_id: self.memory_scope.sender_id.clone(),
                raw_sender: self.memory_scope.raw_sender.clone(),
                owner_id: self.memory_scope.owner_id.clone(),
                require_owner_match: self.memory_scope.require_owner_match,
            },
        };
        Ok(serde_json::to_string(&fields)?)
    }

    pub(crate) fn bind_operator_process(
        &self,
        publication: uuid::Uuid,
        key: &DecisionKey,
    ) -> anyhow::Result<ParentSourceSnapshot> {
        let payload = self.process_payload()?;
        let seal = key.hmac(
            KeyDomain::StateFingerprint,
            &[
                b"operator-worker-source-v1",
                publication.to_string().as_bytes(),
                payload.as_bytes(),
            ],
        );
        Ok(ParentSourceSnapshot { payload, seal })
    }

    pub(crate) fn restore_operator_process(
        worker: &crate::session_worker::protocol::WorkerManifest,
        publication: &crate::runtime::operator_spawn::process::VerifiedOperatorProcessPublication,
        key: &DecisionKey,
    ) -> anyhow::Result<Self> {
        let source = worker
            .operator_source
            .as_ref()
            .context("operator worker source missing")?;
        let snapshot = &source.operator_source;
        anyhow::ensure!(
            worker.ordinary_source.is_none()
                && worker.child_source.is_none()
                && worker.capability_manifest.is_none()
                && key.verify_hmac(
                    KeyDomain::StateFingerprint,
                    &[
                        b"operator-worker-source-v1",
                        publication.id().to_string().as_bytes(),
                        snapshot.payload.as_bytes()
                    ],
                    &snapshot.seal
                ),
            "operator worker source signature is invalid"
        );
        let fields: SourceFields = serde_json::from_str(&snapshot.payload)?;
        anyhow::ensure!(
            fields.surface == "interactive"
                && fields.policy_channel == "terminal"
                && fields.policy_sender == "local-user"
                && !fields.run_id.is_empty()
                && !fields.session_key.is_empty()
                && fields.run_id != worker.run_id
                && fields.workspace_id == publication.workspace()
                && fields.owner_id == publication.owner()
                && worker.parent_run_id.as_deref() == Some(fields.run_id.as_str())
                && worker.owner_id.as_deref() == Some(fields.owner_id.as_str())
                && worker.session_scope_key == fields.session_key
                && worker.scope_channel.as_deref() == Some(fields.policy_channel.as_str())
                && worker.scope_sender.as_deref() == Some(fields.policy_sender.as_str())
                && worker.scope_chat_type.as_deref() == Some(fields.policy_chat_type.as_str())
                && worker.scope_chat_id == fields.memory_scope.chat_id
                && worker.runtime_config_generation_id == Some(fields.config_generation_id)
                && fields.config_generation_id == source.config_generation.id.0
                && worker.runtime_config_source_revision.as_deref() == Some(fields.config_source_revision.as_str())
                && fields.config_source_revision == source.config_generation.source_revision.fingerprint_sha256
                && fields.source_event_id.as_deref().is_none_or(|id| !id.is_empty()),
            "operator worker provenance differs from signed publication"
        );
        Ok(Self {
            owner_isolated: fields.owner_isolated,
            workspace_id: fields.workspace_id,
            owner_id: fields.owner_id,
            session_key: fields.session_key,
            run_id: fields.run_id,
            routing_input: fields.routing_input,
            config_source_revision: fields.config_source_revision,
            source_event_id: fields.source_event_id,
            config_generation_id: fields.config_generation_id,
            policy_channel: fields.policy_channel,
            policy_sender: fields.policy_sender,
            policy_chat_type: fields.policy_chat_type,
            surface: TurnSurfaceKind::Interactive,
            memory_scope: MemoryWriteContext {
                workspace_id: fields.memory_scope.workspace_id,
                channel: fields.memory_scope.channel,
                chat_type: fields.memory_scope.chat_type,
                chat_id: fields.memory_scope.chat_id,
                sender_id: fields.memory_scope.sender_id,
                raw_sender: fields.memory_scope.raw_sender,
                owner_id: fields.memory_scope.owner_id,
                require_owner_match: fields.memory_scope.require_owner_match,
            },
        })
    }

    /// Restore signed ordinary provenance without asserting that a parent
    /// decision was persisted. The scope key must come from the authenticated
    /// worker channel; this value alone is not an executable source proof.
    pub(crate) fn restore_ordinary_process(
        worker: &crate::session_worker::protocol::WorkerManifest,
        scope: &crate::tools::capability::child::VerifiedChildScope,
        generation: &crate::config::ConfigGeneration,
    ) -> anyhow::Result<Self> {
        let source = worker
            .ordinary_source
            .as_ref()
            .context("ordinary worker source missing")?;
        let snapshot = &source.parent_source;
        let (child, constraints) = scope.gate_inputs();
        anyhow::ensure!(
            !generation.effective.causal_tree.enabled
                && worker.child_source.is_none()
                && worker.capability_manifest.is_none()
                && constraints.key.verify_hmac(
                    KeyDomain::StateFingerprint,
                    &[
                        b"worker-parent-source-v1",
                        child.parent_decision_id.to_string().as_bytes(),
                        snapshot.payload.as_bytes()
                    ],
                    &snapshot.seal,
                ),
            "ordinary worker parent provenance signature is invalid"
        );
        let fields: SourceFields = serde_json::from_str(&snapshot.payload)?;
        let surface = match fields.surface.as_str() {
            "interactive" => TurnSurfaceKind::Interactive,
            "agent_loop" => TurnSurfaceKind::AgentLoop,
            "cron" => TurnSurfaceKind::Cron,
            "xin" => TurnSurfaceKind::Xin,
            "server" => TurnSurfaceKind::Server,
            "channel" => TurnSurfaceKind::Channel,
            "worker" => TurnSurfaceKind::Worker,
            _ => anyhow::bail!("ordinary worker parent surface is invalid"),
        };
        anyhow::ensure!(
            !fields.run_id.is_empty()
                && !fields.session_key.is_empty()
                && fields.run_id != worker.run_id
                && fields.workspace_id == constraints.authority.workspace
                && fields.workspace_id == generation.effective.workspace_dir.to_string_lossy()
                && fields.owner_id == constraints.authority.owner
                && fields.config_generation_id == generation.id.0
                && fields.config_source_revision == generation.source_revision.fingerprint_sha256
                && worker.parent_run_id.as_deref() == Some(fields.run_id.as_str())
                && worker.owner_id.as_deref() == Some(fields.owner_id.as_str())
                && worker.scope_channel.as_deref() == Some(fields.policy_channel.as_str())
                && worker.scope_sender.as_deref() == Some(fields.policy_sender.as_str())
                && worker.scope_chat_type.as_deref() == Some(fields.policy_chat_type.as_str())
                && fields.source_event_id.as_deref().is_none_or(|id| !id.is_empty()),
            "ordinary worker parent provenance differs from verified scope"
        );
        Ok(Self {
            owner_isolated: fields.owner_isolated,
            workspace_id: fields.workspace_id,
            owner_id: fields.owner_id,
            session_key: fields.session_key,
            run_id: fields.run_id,
            routing_input: fields.routing_input,
            config_source_revision: fields.config_source_revision,
            source_event_id: fields.source_event_id,
            config_generation_id: fields.config_generation_id,
            policy_channel: fields.policy_channel,
            policy_sender: fields.policy_sender,
            policy_chat_type: fields.policy_chat_type,
            surface,
            memory_scope: MemoryWriteContext {
                workspace_id: fields.memory_scope.workspace_id,
                channel: fields.memory_scope.channel,
                chat_type: fields.memory_scope.chat_type,
                chat_id: fields.memory_scope.chat_id,
                sender_id: fields.memory_scope.sender_id,
                raw_sender: fields.memory_scope.raw_sender,
                owner_id: fields.memory_scope.owner_id,
                require_owner_match: fields.memory_scope.require_owner_match,
            },
        })
    }

    pub(crate) async fn restore_process(
        worker: &crate::session_worker::protocol::WorkerManifest,
        store: &crate::causal_tree::store::CausalDecisionStore,
        scope: &crate::tools::capability::child::VerifiedChildScope,
        fabric: &MemoryFabric,
        generation: &crate::config::ConfigGeneration,
        child_input: &ChildInputReceipt,
    ) -> anyhow::Result<Self> {
        use crate::causal_tree::store::DecisionScope;
        let source = worker.child_source.as_ref().context("worker parent source missing")?;
        let snapshot = &source.parent_source;
        let (child, constraints) = scope.gate_inputs();
        anyhow::ensure!(
            constraints.key.verify_hmac(
                KeyDomain::StateFingerprint,
                &[
                    b"worker-parent-source-v1",
                    child.parent_decision_id.to_string().as_bytes(),
                    snapshot.payload.as_bytes()
                ],
                &snapshot.seal
            ),
            "worker parent provenance signature is invalid"
        );
        let fields: SourceFields = serde_json::from_str(&snapshot.payload)?;
        let surface = match fields.surface.as_str() {
            "interactive" => TurnSurfaceKind::Interactive,
            "agent_loop" => TurnSurfaceKind::AgentLoop,
            "cron" => TurnSurfaceKind::Cron,
            "xin" => TurnSurfaceKind::Xin,
            "server" => TurnSurfaceKind::Server,
            "channel" => TurnSurfaceKind::Channel,
            "worker" => TurnSurfaceKind::Worker,
            _ => anyhow::bail!("worker parent source surface is invalid"),
        };
        let source_id = fields
            .source_event_id
            .as_deref()
            .context("worker parent source identity missing")?;
        anyhow::ensure!(
            !source_id.is_empty()
                && !fields.run_id.is_empty()
                && !fields.session_key.is_empty()
                && fields.workspace_id == fabric.workspace_id()
                && fields.workspace_id == constraints.authority.workspace
                && fields.workspace_id == generation.effective.workspace_dir.to_string_lossy()
                && fields.owner_id == constraints.authority.owner
                && fields.config_generation_id == generation.id.0
                && fields.config_source_revision == generation.source_revision.fingerprint_sha256,
            "worker parent provenance differs from verified scope"
        );
        let store_workspace = store.workspace_id().context("worker parent store namespace missing")?;
        anyhow::ensure!(
            store_workspace.as_ref() == source.parent_store_workspace,
            "worker parent store namespace differs from signed source"
        );
        let owner_scope = DecisionScope::Owner {
            workspace_id: store_workspace,
            owner_id: Arc::from(fields.owner_id.as_str()),
        };
        let row = store
            .reader()
            .replay_record(&owner_scope, &child.parent_decision_id)?
            .context("worker parent decision is unavailable")?;
        anyhow::ensure!(
            row.digest == source.parent_decision_digest
                && row.decision.directive_kind == "delegate"
                && row.decision.enabled_snapshot
                && !row.decision.legacy_v1
                && row.source_event_id.as_deref() == Some(source_id)
                && row.decision.run_id.as_deref() == Some(fields.run_id.as_str())
                && row.decision.session_key.as_deref() == Some(fields.session_key.as_str()),
            "worker parent provenance differs from durable decision"
        );
        let event = fabric
            .memory()
            .find_owned_message_event(&fields.workspace_id, &fields.owner_id, source_id)
            .await?
            .context("worker parent event is unavailable")?;
        let input = child_input.event();
        anyhow::ensure!(
            event.id > 0
                && event.event_id == source_id
                && event.workspace_id == fields.workspace_id
                && event.owner_id.as_deref() == Some(fields.owner_id.as_str())
                && event.role == "user"
                && event.event_type == "message.created"
                && event.run_id.as_deref() == Some(fields.run_id.as_str())
                && event.session_key.as_deref() == Some(fields.session_key.as_str())
                && crate::runtime::turn_source::recorded_input_matches(surface, &event, &fields.routing_input)
                && input.event_id == source.input_event_id
                && input.run_id.as_deref() == Some(worker.run_id.as_str())
                && input.session_key.as_deref() == Some(worker.session_scope_key.as_str())
                && child_input.input() == worker.task
                && input.workspace_id == fields.workspace_id
                && input.owner_id == event.owner_id
                && input.parent_run_id == event.run_id
                && input.parent_session_key == event.session_key
                && input.causation_event_id.as_deref() == Some(source_id)
                && input.run_id != event.run_id
                && input.channel.as_deref() == Some(fields.policy_channel.as_str())
                && input.sender.as_deref() == Some(fields.policy_sender.as_str())
                && input.config_generation_id == Some(fields.config_generation_id)
                && input.config_source_revision.as_deref() == Some(fields.config_source_revision.as_str()),
            "worker parent and child durable lineage differ"
        );
        Ok(Self {
            owner_isolated: fields.owner_isolated,
            workspace_id: fields.workspace_id,
            owner_id: fields.owner_id,
            session_key: fields.session_key,
            run_id: fields.run_id,
            routing_input: fields.routing_input,
            config_source_revision: fields.config_source_revision,
            source_event_id: fields.source_event_id,
            config_generation_id: fields.config_generation_id,
            policy_channel: fields.policy_channel,
            policy_sender: fields.policy_sender,
            policy_chat_type: fields.policy_chat_type,
            surface,
            memory_scope: MemoryWriteContext {
                workspace_id: fields.memory_scope.workspace_id,
                channel: fields.memory_scope.channel,
                chat_type: fields.memory_scope.chat_type,
                chat_id: fields.memory_scope.chat_id,
                sender_id: fields.memory_scope.sender_id,
                raw_sender: fields.memory_scope.raw_sender,
                owner_id: fields.memory_scope.owner_id,
                require_owner_match: fields.memory_scope.require_owner_match,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent(owner_isolated: bool) -> ParentSource {
        ParentSource {
            owner_isolated,
            workspace_id: "prx_workspace".to_owned(),
            owner_id: "prx_user".to_owned(),
            session_key: "prx_session".to_owned(),
            run_id: "prx_parent_run".to_owned(),
            routing_input: "read project notes".to_owned(),
            config_source_revision: "prx_revision".to_owned(),
            source_event_id: Some("prx_event".to_owned()),
            config_generation_id: 1,
            policy_channel: "console".to_owned(),
            policy_sender: "prx_user".to_owned(),
            policy_chat_type: "private".to_owned(),
            surface: TurnSurfaceKind::Server,
            // A memory write scope is not the execution isolation bit.
            memory_scope: MemoryWriteContext::default(),
        }
    }

    #[test]
    fn parent_snapshots_bind_explicit_execution_isolation() {
        let key = DecisionKey::ephemeral();
        let decision = crate::causal_tree::ids::DecisionId::new_runtime();
        let publication = uuid::Uuid::new_v4();
        for owner_isolated in [false, true] {
            let parent = parent(owner_isolated);
            assert_eq!(parent.owner_isolated(), owner_isolated);
            assert!(!parent.memory_scope().require_owner_match);
            let snapshots = [
                (
                    parent.bind_process(decision, &key).unwrap(),
                    b"worker-parent-source-v1".as_slice(),
                    decision.to_string(),
                ),
                (
                    parent.bind_operator_process(publication, &key).unwrap(),
                    b"operator-worker-source-v1".as_slice(),
                    publication.to_string(),
                ),
            ];
            for (snapshot, domain, binding) in snapshots {
                let mut fields: SourceFields = serde_json::from_str(&snapshot.payload).unwrap();
                assert_eq!(fields.owner_isolated, owner_isolated);
                assert!(key.verify_hmac(
                    KeyDomain::StateFingerprint,
                    &[domain, binding.as_bytes(), snapshot.payload.as_bytes()],
                    &snapshot.seal
                ));
                assert_eq!(serde_json::to_string(&fields).unwrap(), snapshot.payload);
                fields.owner_isolated = !owner_isolated;
                let changed_payload = serde_json::to_string(&fields).unwrap();
                assert!(!key.verify_hmac(
                    KeyDomain::StateFingerprint,
                    &[domain, binding.as_bytes(), changed_payload.as_bytes()],
                    &snapshot.seal
                ));
                let mut changed: serde_json::Value = serde_json::from_str(&snapshot.payload).unwrap();
                changed.as_object_mut().unwrap().remove("owner_isolated");
                assert!(serde_json::from_value::<SourceFields>(changed).is_err());
            }
        }
    }
}
