//! Durable batch membership; no execution authority is reconstructed from it.
use super::{BatchMember, BatchRoster};
use crate::decision::{DecisionKey, key::KeyDomain};
use crate::memory::{MemoryFabric, MemoryVisibility, MessageEventScope};
use crate::runtime::child_source::IssuedChildGrant;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DispatchState {
    Started,
    Rejected,
    Unknown,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    run_id: String,
    task: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrdinarySource {
    parent_run: String,
    parent_session: String,
    config_generation: u64,
    config_revision: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fields {
    version: u8,
    workspace: String,
    // Omitted for historical envelopes and the ordinary (non-decision) path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    store_workspace: Option<String>,
    owner: String,
    batch: String,
    parent_decision: String,
    parent_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ordinary_source: Option<OrdinarySource>,
    members: Vec<Member>,
    // None means dispatch did not finish recording. Unknown members remain in
    // the roster; only explicit pre-launch rejections are excluded.
    dispatched: Option<Vec<DispatchState>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signed {
    fields: Fields,
    seal: [u8; 32],
}

pub(super) struct PreparedBatch {
    fabric: MemoryFabric,
    key: Arc<DecisionKey>,
    fields: Fields,
}

impl PreparedBatch {
    pub(super) async fn prepare(
        fabric: MemoryFabric,
        grant: &IssuedChildGrant,
        batch: &str,
        members: &[(String, serde_json::Value)],
    ) -> anyhow::Result<Self> {
        let (scope, constraints) = grant.scope().gate_inputs();
        anyhow::ensure!(
            fabric.workspace_id() == scope.authority.workspace,
            "batch workspace differs from grant"
        );
        let fields = Fields {
            version: 3,
            workspace: fabric.workspace_id().to_owned(),
            store_workspace: Some(grant.parent_record_binding().0.to_owned()),
            owner: scope.authority.owner.clone(),
            batch: batch.to_owned(),
            parent_decision: scope.parent_decision_id.to_string(),
            parent_digest: grant.parent_record_binding().1.to_owned(),
            ordinary_source: None,
            members: members
                .iter()
                .map(|(run_id, args)| Member {
                    run_id: run_id.clone(),
                    task: crate::agent::terminal::redact_persisted_text(
                        args.get("task").and_then(serde_json::Value::as_str).unwrap_or_default(),
                    ),
                })
                .collect(),
            dispatched: None,
        };
        let prepared = Self {
            fabric,
            key: Arc::clone(&constraints.key),
            fields,
        };
        prepared.persist("plan").await?;
        Ok(prepared)
    }

    pub(super) async fn prepare_ordinary(
        publication: &crate::runtime::turn_decision::OrdinaryChildPublication,
        caller: &crate::tools::ToolExecutionContext,
        batch: &str,
        members: &[(String, serde_json::Value)],
    ) -> anyhow::Result<Option<Self>> {
        publication.verify_caller(caller)?;
        let generation = publication.generation();
        if !generation.effective.memory.event_recording_config().enabled {
            return Ok(None);
        }
        let fabric = publication.source_fabric();
        let source = OrdinarySource {
            parent_run: caller
                .envelope
                .run_id
                .clone()
                .context("ordinary batch parent run missing")?,
            parent_session: caller.envelope.session_key.clone(),
            config_generation: generation.id.0,
            config_revision: generation.source_revision.fingerprint_sha256.clone(),
        };
        let key = DecisionKey::from_worker_channel(
            &super::session_worker_signing_key(),
            fabric.workspace_id(),
            batch,
            &source.config_revision,
        )?;
        let fields = Fields {
            version: 2,
            workspace: fabric.workspace_id().to_owned(),
            store_workspace: None,
            owner: caller.envelope.resolved_owner_id(),
            batch: batch.to_owned(),
            parent_decision: String::new(),
            parent_digest: String::new(),
            ordinary_source: Some(source),
            members: members
                .iter()
                .map(|(run_id, args)| Member {
                    run_id: run_id.clone(),
                    task: crate::agent::terminal::redact_persisted_text(
                        args.get("task").and_then(serde_json::Value::as_str).unwrap_or_default(),
                    ),
                })
                .collect(),
            dispatched: None,
        };
        let prepared = Self { fabric, key, fields };
        prepared.persist("plan").await?;
        publication.verify_caller(caller)?;
        Ok(Some(prepared))
    }

    pub(super) async fn finish(mut self, dispatched: Vec<DispatchState>) -> anyhow::Result<()> {
        anyhow::ensure!(
            dispatched.len() == self.fields.members.len(),
            "batch dispatch inventory differs from plan"
        );
        self.fields.dispatched = Some(dispatched);
        self.persist("dispatch").await
    }

    async fn persist(&self, phase: &str) -> anyhow::Result<()> {
        let seal = self.key.hmac(
            KeyDomain::StateFingerprint,
            &[b"child-batch-journal-v1", &serde_json::to_vec(&self.fields)?],
        );
        let payload = serde_json::to_string(&Signed {
            fields: self.fields.clone(),
            seal,
        })?;
        let lookup = format!("batch:{}:{phase}", self.fields.batch);
        let event_type = format!("worker.batch.{phase}");
        let event = self
            .fabric
            .record_runtime_event_idempotent(
                MessageEventScope::new("sessions_spawn", MemoryVisibility::Workspace)
                    .with_owner_id(&self.fields.owner)
                    .with_run_id(&self.fields.batch),
                &event_type,
                "worker batch membership",
                Some(payload.clone()),
                lookup.clone(),
            )
            .await?;
        anyhow::ensure!(
            event.id > 0
                && event.raw_payload_json.as_deref() == Some(payload.as_str())
                && event.idempotency_key.as_deref() == Some(lookup.as_str())
                && event.owner_id.as_deref() == Some(self.fields.owner.as_str())
                && event.workspace_id == self.fields.workspace
                && event.event_type == event_type,
            "batch journal receipt differs from plan"
        );
        let stored = self
            .fabric
            .memory()
            .find_owned_message_event(&self.fields.workspace, &self.fields.owner, &event.event_id)
            .await?
            .context("batch journal is not durable")?;
        anyhow::ensure!(
            serde_json::to_value(&stored)? == serde_json::to_value(&event)?,
            "batch journal readback differs"
        );
        verify(&stored, &self.fields.workspace, &self.fields.batch, phase, &self.key)?;
        Ok(())
    }
}

pub(super) async fn read(
    fabric: &MemoryFabric,
    workspace: &std::path::Path,
    batch: &str,
    owners: &crate::tools::sessions_read_model::RunOwnerScope,
    expected_parent: Option<(crate::causal_tree::ids::DecisionId, &str)>,
) -> anyhow::Result<Option<BatchRoster>> {
    anyhow::ensure!(
        fabric.workspace_id() == workspace.to_string_lossy(),
        "batch lookup workspace differs"
    );
    let memory = fabric.memory();
    let lookup = format!("batch:{batch}:plan");
    let Some(plan) = memory
        .find_message_event_by_idempotency_key(fabric.workspace_id(), &lookup)
        .await?
    else {
        return Ok(None);
    };
    if !owners.admits(plan.owner_id.as_deref()) {
        return Ok(None);
    }
    let parsed: Signed = serde_json::from_str(plan.raw_payload_json.as_deref().context("batch payload missing")?)?;
    let key = if let Some(source) = parsed.fields.ordinary_source.as_ref() {
        anyhow::ensure!(
            expected_parent.is_none(),
            "ordinary batch cannot satisfy a durable parent grant"
        );
        DecisionKey::from_worker_channel(
            &super::existing_session_worker_signing_key()?,
            fabric.workspace_id(),
            batch,
            &source.config_revision,
        )?
    } else {
        DecisionKey::load_existing(workspace)?
    };
    let plan_fields = verify(&plan, fabric.workspace_id(), batch, "plan", &key)?;
    anyhow::ensure!(
        plan_fields.dispatched.is_none(),
        "batch plan contains a dispatch result"
    );
    if plan_fields.ordinary_source.is_none() {
        let decision = crate::causal_tree::ids::DecisionId::parse(&plan_fields.parent_decision)?;
        if let Some((expected, digest)) = expected_parent {
            anyhow::ensure!(
                decision == expected && plan_fields.parent_digest == digest,
                "batch belongs to another parent decision"
            );
        }
        anyhow::ensure!(
            decision.to_string() == plan_fields.parent_decision,
            "batch parent identity is not canonical"
        );
        let original_store = plan_fields
            .store_workspace
            .as_deref()
            .filter(|_| plan_fields.version == 3)
            .context("legacy child batch lacks its original store namespace; membership remains unconfirmed")?;
        let store = crate::causal_tree::store::CausalDecisionStore::open_unshared_read_only(workspace);
        let store_workspace = store.workspace_id().context("batch decision store namespace missing")?;
        anyhow::ensure!(
            store_workspace.as_ref() == original_store,
            "batch original store namespace differs from configured store"
        );
        let scope = crate::causal_tree::store::DecisionScope::Owner {
            workspace_id: store_workspace,
            owner_id: Arc::from(plan_fields.owner.as_str()),
        };
        let parent = store
            .reader()
            .replay_record(&scope, &decision)?
            .context("batch parent decision missing")?;
        anyhow::ensure!(
            parent.digest == plan_fields.parent_digest
                && parent.decision.enabled_snapshot
                && !parent.decision.legacy_v1
                && parent.decision.directive_kind == "delegate",
            "batch parent differs from signed plan"
        );
    }
    let dispatched = if let Some(event) = memory
        .find_owned_message_event_by_idempotency_key(
            fabric.workspace_id(),
            &plan_fields.owner,
            &format!("batch:{batch}:dispatch"),
        )
        .await?
    {
        let mut fields = verify(&event, fabric.workspace_id(), batch, "dispatch", &key)?;
        let result = fields.dispatched.take().context("batch dispatch result missing")?;
        anyhow::ensure!(
            serde_json::to_value(&fields)? == serde_json::to_value(&plan_fields)?
                && result.len() == fields.members.len(),
            "batch dispatch differs from signed plan"
        );
        Some(result)
    } else {
        None
    };
    Ok(Some(BatchRoster {
        batch_id: batch.to_owned(),
        owner_id: Some(plan_fields.owner),
        members: plan_fields
            .members
            .into_iter()
            .enumerate()
            .filter(|(index, _)| {
                dispatched.as_ref().is_none_or(|states| {
                    states
                        .get(*index)
                        .is_some_and(|state| *state != DispatchState::Rejected)
                })
            })
            .map(|(_, member)| BatchMember {
                run_id: member.run_id,
                task: member.task,
            })
            .collect(),
    }))
}

fn verify(
    event: &crate::memory::MessageEvent,
    workspace: &str,
    batch: &str,
    phase: &str,
    key: &DecisionKey,
) -> anyhow::Result<Fields> {
    let signed: Signed = serde_json::from_str(event.raw_payload_json.as_deref().context("batch payload missing")?)?;
    anyhow::ensure!(
        key.verify_hmac(
            KeyDomain::StateFingerprint,
            &[b"child-batch-journal-v1", &serde_json::to_vec(&signed.fields)?],
            &signed.seal
        ),
        "batch journal signature invalid"
    );
    let fields = signed.fields;
    anyhow::ensure!(
        (match &fields.ordinary_source {
            Some(source) =>
                fields.version == 2
                    && fields.store_workspace.is_none()
                    && fields.parent_decision.is_empty()
                    && fields.parent_digest.is_empty()
                    && !source.parent_run.is_empty()
                    && !source.parent_session.is_empty()
                    && !source.config_revision.is_empty(),
            None => matches!(fields.version, 1 | 3),
        }) && fields.workspace == workspace
            && fields.batch == batch
            && !fields.owner.is_empty()
            && event.id > 0
            && event.workspace_id == workspace
            && event.owner_id.as_deref() == Some(fields.owner.as_str())
            && event.run_id.as_deref() == Some(batch)
            && event.source == crate::memory::MessageEventSource::SessionsSpawn
            && event.role == "event"
            && event.visibility == MemoryVisibility::Workspace
            && event.event_type == format!("worker.batch.{phase}")
            && event.idempotency_key.as_deref() == Some(format!("batch:{batch}:{phase}").as_str()),
        "batch journal scope differs"
    );
    let mut ids = std::collections::HashSet::new();
    for member in &fields.members {
        anyhow::ensure!(
            uuid::Uuid::parse_str(&member.run_id)?.to_string() == member.run_id && ids.insert(&member.run_id),
            "batch member identity is invalid or duplicated"
        );
    }
    Ok(fields)
}

/// Validate read-only ordinary membership against the authenticated parent
/// lineage. A durable decision roster cannot be substituted for this source.
pub(super) async fn verify_ordinary_member(
    fabric: &MemoryFabric,
    batch: &str,
    owner: &str,
    parent_run: &str,
    parent_session: &str,
    run: &str,
) -> anyhow::Result<()> {
    let lookup = format!("batch:{batch}:plan");
    let event = fabric
        .memory()
        .find_owned_message_event_by_idempotency_key(fabric.workspace_id(), owner, &lookup)
        .await?
        .context("ordinary batch plan missing")?;
    let parsed: Signed = serde_json::from_str(
        event
            .raw_payload_json
            .as_deref()
            .context("ordinary batch payload missing")?,
    )?;
    let source = parsed
        .fields
        .ordinary_source
        .as_ref()
        .context("ordinary launch requires ordinary batch provenance")?;
    let key = DecisionKey::from_worker_channel(
        &super::existing_session_worker_signing_key()?,
        fabric.workspace_id(),
        batch,
        &source.config_revision,
    )?;
    let fields = verify(&event, fabric.workspace_id(), batch, "plan", &key)?;
    let source = fields
        .ordinary_source
        .as_ref()
        .context("ordinary batch source missing")?;
    anyhow::ensure!(
        fields.owner == owner
            && source.parent_run == parent_run
            && source.parent_session == parent_session
            && fields.members.iter().any(|member| member.run_id == run),
        "ordinary worker differs from batch parent or membership"
    );
    let roster = read(
        fabric,
        std::path::Path::new(fabric.workspace_id()),
        batch,
        &crate::tools::sessions_read_model::RunOwnerScope::Owner(Some(owner.to_owned())),
        None,
    )
    .await?
    .context("ordinary batch roster missing")?;
    anyhow::ensure!(
        roster.members.iter().any(|member| member.run_id == run),
        "ordinary worker was rejected by batch dispatch"
    );
    Ok(())
}
