//! Signed operator capability ceiling. This is transport authority, not a turn.
mod snapshots;
use crate::causal_tree::branch::CapabilityRef;
use crate::decision::key::{DecisionKey, KeyDomain};
use crate::session_worker::protocol::WorkerOperatorPublication;
use crate::tools::capability::catalog::CapabilityCatalog;
use std::collections::BTreeSet;
use subtle::ConstantTimeEq;

pub(crate) struct VerifiedOperatorProcessPublication {
    id: uuid::Uuid,
    owner: String,
    workspace: String,
    level: crate::security::AutonomyLevel,
    max_cost_cents: u32,
    allowed: BTreeSet<CapabilityRef>,
}

impl WorkerOperatorPublication {
    fn signing_payload(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(
            &self.publication_id,
            &self.owner,
            &self.workspace,
            self.level,
            self.max_cost_cents,
            &self.allowed,
            self.catalog_generation,
        ))?)
    }

    pub(crate) fn sign(&mut self, key: &DecisionKey) -> anyhow::Result<()> {
        self.seal = key.hmac(
            KeyDomain::StateFingerprint,
            &[b"operator-worker-publication-v1", &self.signing_payload()?],
        );
        Ok(())
    }

    pub(crate) fn verify(
        &self,
        key: &DecisionKey,
        catalog: &CapabilityCatalog,
    ) -> anyhow::Result<VerifiedOperatorProcessPublication> {
        let expected = key.hmac(
            KeyDomain::StateFingerprint,
            &[b"operator-worker-publication-v1", &self.signing_payload()?],
        );
        anyhow::ensure!(
            bool::from(expected.ct_eq(&self.seal)),
            "operator publication seal differs"
        );
        let id = uuid::Uuid::parse_str(&self.publication_id)?;
        anyhow::ensure!(
            id.to_string() == self.publication_id
                && !self.owner.is_empty()
                && !self.workspace.is_empty()
                && catalog.generation().0 == self.catalog_generation,
            "operator publication identity or catalog differs"
        );
        let view = catalog.view(&crate::tools::capability::catalog::CatalogScope::default());
        let mut allowed = BTreeSet::new();
        for wire in &self.allowed {
            let entry = view
                .iter_entries()
                .find(|entry| entry.id.as_str() == wire.id)
                .ok_or_else(|| anyhow::anyhow!("operator capability is absent or unavailable"))?;
            anyhow::ensure!(
                entry.fingerprint.0 == wire.fingerprint
                    && entry.generation.0 == wire.generation
                    && wire.generation == self.catalog_generation
                    && entry.availability.level >= crate::capability::CapabilityAvailabilityLevel::Configured,
                "operator capability reference differs from signed catalog"
            );
            anyhow::ensure!(
                allowed.insert(CapabilityRef {
                    id: entry.id.clone(),
                    fingerprint: entry.fingerprint,
                    generation: entry.generation,
                }),
                "operator capability reference is duplicated"
            );
        }
        Ok(VerifiedOperatorProcessPublication {
            id,
            owner: self.owner.clone(),
            workspace: self.workspace.clone(),
            level: self.level,
            max_cost_cents: self.max_cost_cents,
            allowed,
        })
    }
}

impl VerifiedOperatorProcessPublication {
    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }
    pub(crate) const fn id(&self) -> uuid::Uuid {
        self.id
    }
    pub(crate) fn workspace(&self) -> &str {
        &self.workspace
    }
    pub(crate) const fn allowed(&self) -> &BTreeSet<CapabilityRef> {
        &self.allowed
    }
}

pub(crate) struct OperatorProcessLaunch {
    source: std::sync::Arc<super::OperatorSpawnSource>,
    scope: crate::memory::MessageEventScope,
    receipt: Option<std::sync::Arc<crate::runtime::child_source::ChildInputReceipt>>,
    config_generation: String,
    id: uuid::Uuid,
}

impl super::OperatorSpawnSource {
    pub(crate) fn capture_process(
        self: &std::sync::Arc<Self>,
        scope: &crate::memory::MessageEventScope,
        input: &str,
        receipt: Option<std::sync::Arc<crate::runtime::child_source::ChildInputReceipt>>,
        config_generation: String,
    ) -> anyhow::Result<OperatorProcessLaunch> {
        use crate::runtime::turn_source::TurnSourceProof;
        use std::sync::atomic::Ordering;
        self.verify_live()?;
        anyhow::ensure!(
            self.claimed.load(Ordering::Acquire)
                && input == self.task.trim()
                && !config_generation.is_empty()
                && scope.source == crate::memory::MessageEventSource::SessionsSpawn
                && scope
                    .run_id
                    .as_deref()
                    .is_some_and(|run| !run.is_empty() && run != self.proof.run_id())
                && scope.owner_id.as_deref() == Some(self.proof.owner_id())
                && scope.session_key.as_deref() == Some(self.proof.session_key())
                && scope.parent_run_id.as_deref() == Some(self.proof.run_id())
                && scope.parent_session_key == scope.session_key
                && scope.channel.as_deref() == Some(self.proof.policy_channel())
                && scope.sender.as_deref() == Some(self.proof.policy_sender())
                && scope.recipient == self.proof.memory_scope().chat_id
                && scope.config_generation_id == Some(self.generation.id.0)
                && scope.config_source_revision.as_deref() == Some(self.proof.config_source_revision()),
            "operator process input differs from admitted command"
        );
        let recording = self.generation.effective.memory.event_recording_config();
        if let Some(receipt) = &receipt {
            receipt.verify_backend(&self.fabric.memory())?;
            let event = receipt.event();
            anyhow::ensure!(
                event.id > 0
                    && receipt.input() == input
                    && event.content == input
                    && event.workspace_id == self.fabric.workspace_id()
                    && event.source == scope.source
                    && event.owner_id == scope.owner_id
                    && event.run_id == scope.run_id
                    && event.session_key == scope.session_key
                    && event.parent_session_key == scope.parent_session_key
                    && event.parent_run_id == scope.parent_run_id
                    && event.channel == scope.channel
                    && event.sender == scope.sender
                    && event.recipient == scope.recipient
                    && event.agent_id == scope.agent_id
                    && event.persona_id == scope.persona_id
                    && event.role == "user"
                    && event.event_type == "message.created"
                    && event.visibility == scope.visibility
                    && event.goal_id == scope.goal_id
                    && event.correlation_id == scope.correlation_id
                    && event.subject == scope.subject
                    && event.causation_event_id == scope.causation_event_id
                    && event.attempt_id == scope.attempt_id
                    && event.lease_epoch == scope.lease_epoch
                    && event.config_generation_id == scope.config_generation_id
                    && event.config_source_revision == scope.config_source_revision,
                "operator process receipt differs from admitted input"
            );
        } else {
            anyhow::ensure!(
                !recording.enabled || !recording.record_user_messages,
                "operator process input receipt missing"
            );
        }
        anyhow::ensure!(
            self.child_issued
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "operator command already issued child authority"
        );
        self.verify_live()?;
        Ok(OperatorProcessLaunch {
            source: std::sync::Arc::clone(self),
            scope: scope.clone(),
            receipt,
            config_generation,
            id: uuid::Uuid::now_v7(),
        })
    }
}

impl OperatorProcessLaunch {
    fn bind_worker_manifest(
        self,
        transport_key: &ring::hmac::Key,
        worker: &mut crate::session_worker::protocol::WorkerManifest,
    ) -> anyhow::Result<()> {
        use crate::runtime::turn_source::TurnSourceProof;
        let source = &self.source;
        source.publication.verify(
            &source.proof,
            &source.generation,
            &source.fabric.memory(),
            &source.security,
        )?;
        anyhow::ensure!(
            worker.parent_capability.is_none()
                && worker.child_source.is_none()
                && worker.capability_manifest.is_none()
                && worker.ordinary_source.is_none()
                && worker.operator_source.is_none()
                && worker.task == source.task.trim()
                && Some(worker.run_id.as_str()) == self.scope.run_id.as_deref()
                && worker.config_generation == self.config_generation
                && worker.session_scope_key == source.proof.session_key()
                && worker.owner_id == self.scope.owner_id
                && worker.parent_run_id == self.scope.parent_run_id
                && worker.agent_id == self.scope.agent_id
                && worker.persona_id == self.scope.persona_id
                && worker.scope_channel == self.scope.channel
                && worker.scope_sender == self.scope.sender
                && worker.scope_chat_id == self.scope.recipient
                && worker.scope_chat_type.as_deref() == Some(source.proof.policy_chat_type())
                && worker.runtime_config_generation_id == self.scope.config_generation_id
                && worker.runtime_config_source_revision == self.scope.config_source_revision
                && worker.provider_name == source.resolved.provider_label()
                && worker.model == source.resolved.model()
                && worker.route_hint.as_deref() == source.route.as_ref().map(|route| route.hint.as_str()),
            "operator worker manifest differs from captured launch"
        );
        let key = DecisionKey::from_worker_channel(
            transport_key,
            source.proof.workspace_id(),
            &worker.run_id,
            &worker.config_generation,
        )?;
        let input = self
            .receipt
            .as_ref()
            .map(|receipt| -> anyhow::Result<_> {
                Ok(crate::session_worker::protocol::WorkerRecordedInput {
                    event_id: receipt.event().event_id.clone(),
                    event_mac: key.hmac(
                        KeyDomain::StateFingerprint,
                        &[
                            b"operator-worker-input-v1",
                            self.id.to_string().as_bytes(),
                            &serde_json::to_vec(receipt.event())?,
                        ],
                    ),
                })
            })
            .transpose()?;
        let snapshots = source.publication.bind_process_snapshots(
            self.id,
            &key,
            input,
            crate::session_worker::protocol::WorkerProviderSelection {
                provider: source.proof.provider_name().to_owned(),
                model: source.proof.model_name().to_owned(),
                credential: source.resolved.credential().reference().clone(),
            },
            self.scope.attempt_id.clone(),
            self.scope.lease_epoch,
        )?;
        source.publication.verify(
            &source.proof,
            &source.generation,
            &source.fabric.memory(),
            &source.security,
        )?;
        let root = matches!(
            source.resolved.credential().reference(),
            crate::providers::credentials::CredentialRef::LegacyInline(crate::providers::credentials::LegacySlot::Root)
        );
        worker.api_key = source.resolved.credential().api_key().map(str::to_owned);
        worker.direct_uses_root_connection = source.route.is_none() && root;
        worker.route_uses_root_connection = source.route.is_some() && root;
        worker.route_source_provider = source.route.as_ref().map(|_| source.proof.provider_name().to_owned());
        worker.route_credential = source
            .route
            .as_ref()
            .map(|_| source.resolved.credential().reference().clone());
        worker.operator_source = Some(snapshots);
        Ok(())
    }
}

pub(crate) enum ProcessSourceLaunch {
    Ordinary(crate::runtime::turn_decision::OrdinaryProcessLaunch),
    Operator(OperatorProcessLaunch),
}
impl ProcessSourceLaunch {
    pub(crate) const fn is_ordinary(&self) -> bool {
        matches!(self, Self::Ordinary(_))
    }
    pub(crate) fn generation(&self) -> &std::sync::Arc<crate::config::ConfigGeneration> {
        match self {
            Self::Ordinary(launch) => launch.generation(),
            Self::Operator(launch) => &launch.source.generation,
        }
    }
    pub(crate) fn source_fabric(&self) -> crate::memory::MemoryFabric {
        match self {
            Self::Ordinary(launch) => launch.source_fabric(),
            Self::Operator(launch) => launch.source.fabric.clone(),
        }
    }
    pub(crate) fn persona_id(&self) -> Option<&str> {
        match self {
            Self::Ordinary(launch) => launch.persona_id(),
            Self::Operator(launch) => launch.scope.persona_id.as_deref(),
        }
    }
    pub(crate) fn bind_worker_manifest(
        self,
        key: &ring::hmac::Key,
        worker: &mut crate::session_worker::protocol::WorkerManifest,
    ) -> anyhow::Result<()> {
        match self {
            Self::Ordinary(launch) => launch.bind_worker_manifest(key, worker),
            Self::Operator(launch) => launch.bind_worker_manifest(key, worker),
        }
    }
}

pub(crate) struct OperatorProcessSnapshots {
    pub(crate) publication: VerifiedOperatorProcessPublication,
    pub(crate) key: std::sync::Arc<DecisionKey>,
    pub(crate) catalog: std::sync::Arc<CapabilityCatalog>,
    pub(crate) generation: std::sync::Arc<crate::config::ConfigGeneration>,
    pub(crate) provider_catalog: std::sync::Arc<crate::providers::catalog::CatalogView>,
    pub(crate) parent: crate::runtime::child_source::parent::ParentSource,
    pub(crate) selection: crate::session_worker::protocol::WorkerProviderSelection,
    pub(crate) resolved: crate::providers::catalog::ResolvedModel,
    pub(crate) route: Option<crate::providers::catalog::RouteTarget>,
}

impl VerifiedOperatorProcessPublication {
    pub(crate) fn verify_policy(
        &self,
        security: &crate::security::SecurityPolicy,
        parent: &crate::runtime::child_source::parent::ParentSource,
        catalog: &CapabilityCatalog,
    ) -> anyhow::Result<()> {
        let rank = |level| match level {
            crate::security::AutonomyLevel::ReadOnly => 0,
            crate::security::AutonomyLevel::Supervised => 1,
            crate::security::AutonomyLevel::Full => 2,
        };
        anyhow::ensure!(
            rank(self.level) <= rank(security.autonomy) && self.max_cost_cents <= security.max_cost_per_day_cents,
            "operator process authority exceeds restored security policy"
        );
        let view = catalog.view(&crate::tools::capability::catalog::CatalogScope::default());
        for reference in &self.allowed {
            let entry = view
                .entry_any(&reference.id)
                .ok_or_else(|| anyhow::anyhow!("operator capability missing"))?;
            anyhow::ensure!(
                crate::tools::execution::decide_tool_execution(
                    security,
                    &entry.public_name,
                    parent.policy_sender(),
                    parent.policy_channel(),
                    parent.policy_chat_type()
                ) != crate::tools::execution::ToolExecutionDecision::Deny,
                "operator capability is denied by restored security policy"
            );
        }
        Ok(())
    }
}
