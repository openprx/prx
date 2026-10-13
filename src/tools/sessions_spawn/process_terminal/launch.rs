//! Minimal signed launch provenance for read-only restart reconciliation.
pub(super) mod process_identity;
use crate::memory::{MemoryFabric, MemoryVisibility, MessageEventScope};
use crate::runtime::child_source::{ChildInputReceipt, IssuedChildGrant};
use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchFields {
    version: u8,
    workspace_id: String,
    // Absent in v1. Omission must preserve its original signed canonical form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    store_workspace_id: Option<String>,
    owner_id: String,
    run_id: String,
    session_key: String,
    parent_decision_id: String,
    parent_decision_digest: String,
    input_event_id: String,
    input_event_mac: [u8; 32],
    config_generation_id: u64,
    config_source_revision: String,
    worker_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process_context_mac: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    batch_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchBinding {
    fields: LaunchFields,
    seal: [u8; 32],
}

pub(super) async fn persist(
    fabric: &MemoryFabric,
    grant: &IssuedChildGrant,
    input: &ChildInputReceipt,
    pid: u32,
    batch_id: Option<&str>,
) -> anyhow::Result<()> {
    grant.verify_receipt(input)?;
    let (child, constraints) = grant.scope().gate_inputs();
    let source = input.event();
    let run = source.run_id.as_deref().context("worker launch run missing")?;
    let session = source.session_key.as_deref().context("worker launch session missing")?;
    anyhow::ensure!(
        pid > 0 && fabric.workspace_id() == child.authority.workspace,
        "worker launch identity differs from issued scope"
    );
    let fields = LaunchFields {
        version: 2,
        workspace_id: fabric.workspace_id().to_owned(),
        store_workspace_id: Some(grant.parent_record_binding().0.to_owned()),
        owner_id: child.authority.owner.clone(),
        run_id: run.to_owned(),
        session_key: session.to_owned(),
        parent_decision_id: child.parent_decision_id.to_string(),
        parent_decision_digest: grant.parent_record_binding().1.to_owned(),
        input_event_id: source.event_id.clone(),
        input_event_mac: constraints.key.hmac(
            crate::decision::key::KeyDomain::StateFingerprint,
            &[b"worker-recovery-input-v1", &serde_json::to_vec(source)?],
        ),
        config_generation_id: grant.generation().id.0,
        config_source_revision: grant.generation().source_revision.fingerprint_sha256.clone(),
        worker_pid: pid,
        process_context_mac: process_identity::context_mac(&constraints.key)?,
        batch_id: batch_id.map(str::to_owned),
    };
    let seal = constraints.key.hmac(
        crate::decision::key::KeyDomain::StateFingerprint,
        &[b"worker-recovery-launch-v1", &serde_json::to_vec(&fields)?],
    );
    let payload = serde_json::to_string(&LaunchBinding { fields, seal })?;
    let mut scope = MessageEventScope::new("session_worker", MemoryVisibility::Workspace)
        .with_owner_id(&child.authority.owner)
        .with_run_id(run)
        .with_session_key(session);
    scope.parent_run_id = source.parent_run_id.clone();
    scope.config_generation_id = source.config_generation_id;
    scope.config_source_revision = source.config_source_revision.clone();
    let key = format!("worker:{run}:launch");
    let event = fabric
        .record_runtime_event_idempotent(
            scope,
            "worker.recovery.registered",
            "worker recovery registered",
            Some(payload.clone()),
            key.clone(),
        )
        .await?;
    anyhow::ensure!(
        event.id > 0
            && event.workspace_id == fabric.workspace_id()
            && event.owner_id.as_deref() == Some(child.authority.owner.as_str())
            && event.run_id.as_deref() == Some(run)
            && event.event_type == "worker.recovery.registered"
            && event.idempotency_key.as_deref() == Some(key.as_str())
            && event.raw_payload_json.as_deref() == Some(payload.as_str()),
        "worker launch receipt differs from recovery binding"
    );
    let stored = fabric
        .memory()
        .find_owned_message_event(fabric.workspace_id(), &child.authority.owner, &event.event_id)
        .await?
        .context("worker launch recovery binding is not durable")?;
    anyhow::ensure!(
        serde_json::to_value(&stored)? == serde_json::to_value(&event)?,
        "worker launch recovery binding differs from durable record"
    );
    let verified = read(
        fabric,
        &grant.generation().effective.workspace_dir,
        &child.authority.owner,
        run,
    )
    .await?
    .context("worker launch cannot be verified after persistence")?;
    anyhow::ensure!(
        verified.fields.worker_pid == pid && verified.fields.input_event_id == source.event_id,
        "worker launch verification differs from owned child"
    );
    Ok(())
}

/// Read-only provenance. It implements no execution/source authority trait.
pub(super) struct VerifiedLaunch {
    event_id: i64,
    fields: LaunchFields,
    workspace: std::path::PathBuf,
    input: crate::memory::MessageEvent,
    key: std::sync::Arc<crate::decision::DecisionKey>,
    parent_decision: crate::causal_tree::ids::DecisionId,
    store_workspace_id: String,
}

impl VerifiedLaunch {
    pub(super) fn batch_id(&self) -> Option<&str> {
        self.fields.batch_id.as_deref()
    }
    pub(super) const fn event_id(&self) -> i64 {
        self.event_id
    }

    /// An absent PID is meaningful only in the launch's boot and PID namespace.
    /// Older records and unsupported platforms retain the unconfirmed state.
    pub(super) fn process_absent(&self) -> anyhow::Result<bool> {
        let Some(expected) = self.fields.process_context_mac else {
            return Ok(false);
        };
        if process_identity::context_mac(&self.key)? != Some(expected) {
            return Ok(false);
        }
        process_identity::absent(self.fields.worker_pid)
    }

    pub(super) fn provenance(&self) -> super::RecoveryProvenance<'_> {
        super::RecoveryProvenance {
            workspace: &self.workspace,
            store_workspace: &self.store_workspace_id,
            owner: &self.fields.owner_id,
            parent_decision: self.parent_decision,
            parent_digest: &self.fields.parent_decision_digest,
            input: &self.input,
            key: &self.key,
        }
    }
}

pub(super) async fn read(
    fabric: &MemoryFabric,
    workspace: &std::path::Path,
    owner: &str,
    run: &str,
) -> anyhow::Result<Option<VerifiedLaunch>> {
    use crate::causal_tree::ids::DecisionId;
    use crate::causal_tree::store::{CausalDecisionStore, DecisionScope};
    use crate::decision::key::KeyDomain;
    use crate::memory::MessageEventSource;
    use std::sync::Arc;
    let workspace_matches = workspace.to_string_lossy() == fabric.workspace_id();
    anyhow::ensure!(
        !owner.is_empty() && !run.is_empty() && workspace_matches,
        "worker launch lookup requires exact logical workspace, owner and run"
    );
    let lookup = format!("worker:{run}:launch");
    let memory = fabric.memory();
    let Some(event) = memory
        .find_owned_message_event_by_idempotency_key(fabric.workspace_id(), owner, &lookup)
        .await?
    else {
        return Ok(None);
    };
    let binding: LaunchBinding = serde_json::from_str(
        event
            .raw_payload_json
            .as_deref()
            .context("worker launch payload missing")?,
    )?;
    let key = crate::decision::DecisionKey::load_existing(workspace)?;
    anyhow::ensure!(
        key.verify_hmac(
            KeyDomain::StateFingerprint,
            &[b"worker-recovery-launch-v1", &serde_json::to_vec(&binding.fields)?],
            &binding.seal
        ),
        "worker launch recovery signature is invalid"
    );
    let fields = binding.fields;
    let decision = DecisionId::parse(&fields.parent_decision_id)?;
    anyhow::ensure!(
        matches!(fields.version, 1 | 2)
            && fields.worker_pid > 0
            && fields.workspace_id == fabric.workspace_id()
            && fields.owner_id == owner
            && fields.run_id == run
            && decision.to_string() == fields.parent_decision_id
            && event.id > 0
            && !event.event_id.is_empty()
            && event.workspace_id == fields.workspace_id
            && event.owner_id.as_deref() == Some(owner)
            && event.source == MessageEventSource::SessionWorker
            && event.visibility == MemoryVisibility::Workspace
            && event.role == "event"
            && event.event_type == "worker.recovery.registered"
            && event.run_id.as_deref() == Some(run)
            && event.session_key.as_deref() == Some(fields.session_key.as_str())
            && event.idempotency_key.as_deref() == Some(lookup.as_str())
            && event.config_generation_id == Some(fields.config_generation_id)
            && event.config_source_revision.as_deref() == Some(fields.config_source_revision.as_str()),
        "worker launch record differs from signed provenance"
    );
    let input = memory
        .find_owned_message_event(fabric.workspace_id(), owner, &fields.input_event_id)
        .await?
        .context("worker launch original input missing")?;
    let original_input_matches = input.event_id == fields.input_event_id;
    anyhow::ensure!(
        key.verify_hmac(
            KeyDomain::StateFingerprint,
            &[b"worker-recovery-input-v1", &serde_json::to_vec(&input)?],
            &fields.input_event_mac
        ) && input.id > 0
            && original_input_matches
            && input.workspace_id == fields.workspace_id
            && input.owner_id.as_deref() == Some(owner)
            && input.source == MessageEventSource::SessionsSpawn
            && input.visibility == MemoryVisibility::Workspace
            && input.role == "user"
            && input.event_type == "message.created"
            && input.run_id.as_deref() == Some(run)
            && input.session_key == event.session_key
            && input.parent_run_id == event.parent_run_id
            && input.config_generation_id == event.config_generation_id
            && input.config_source_revision == event.config_source_revision,
        "worker launch original input differs from signed provenance"
    );
    // A historical v1 record can still be decoded and MAC-verified above,
    // but cannot assert which original decision store it belonged to.
    let store_workspace_id = fields
        .store_workspace_id
        .as_deref()
        .filter(|_| fields.version == 2)
        .context("legacy worker launch lacks its original store namespace; recovery remains unconfirmed")?
        .to_owned();
    let store = CausalDecisionStore::open_unshared_read_only(workspace);
    let actual_store = store
        .workspace_id()
        .context("worker launch decision store namespace missing")?;
    anyhow::ensure!(
        actual_store.as_ref() == store_workspace_id,
        "worker launch original store namespace differs from configured store"
    );
    let scope = DecisionScope::Owner {
        workspace_id: actual_store,
        owner_id: Arc::from(owner),
    };
    let parent = store
        .reader()
        .replay_record(&scope, &decision)?
        .context("worker launch parent decision missing")?;
    anyhow::ensure!(
        parent.digest == fields.parent_decision_digest
            && parent.decision.enabled_snapshot
            && !parent.decision.legacy_v1
            && parent.decision.admits_issued_child()
            && parent.decision.run_id.is_some()
            && parent.decision.run_id == input.parent_run_id
            && parent.decision.session_key.is_some()
            && parent.decision.session_key == input.parent_session_key
            && parent.source_event_id.is_some()
            && parent.source_event_id == input.causation_event_id,
        "worker launch parent decision differs from durable provenance"
    );
    if let Some(batch) = fields.batch_id.as_deref() {
        let owner_scope = crate::tools::sessions_read_model::RunOwnerScope::Owner(Some(owner.to_owned()));
        let roster = super::super::batch_journal::read(
            fabric,
            workspace,
            batch,
            &owner_scope,
            Some((decision, &fields.parent_decision_digest)),
        )
        .await?
        .context("worker launch batch plan missing")?;
        anyhow::ensure!(
            roster.members.iter().any(|member| member.run_id == run),
            "worker launch is not an admitted batch member"
        );
    }
    Ok(Some(VerifiedLaunch {
        event_id: event.id,
        fields,
        workspace: workspace.to_path_buf(),
        input,
        key,
        parent_decision: decision,
        store_workspace_id,
    }))
}
