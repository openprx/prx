//! Durable no-operation result of source-bound argument collection.
//! Owns receipt persistence only; cannot issue approval or execution authority.
use super::ApprovalCollectionLease;
use crate::memory::{MemoryFabric, MessageEvent};
use crate::runtime::turn_source::TurnSourceProof;
use anyhow::Context;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) const DECLINED_REPLY: &str = "No operation has been executed or submitted for approval. Are you asking for an explanation of this capability, or requesting an action?";

impl ApprovalCollectionLease {
    pub(crate) async fn decline(self, cancellation: &CancellationToken) -> anyhow::Result<()> {
        anyhow::ensure!(!cancellation.is_cancelled(), "collection cancelled");
        self.service.validate_approval_collection(&self.source, &self.context)?;
        self.service
            .settlement_ref(Arc::clone(&self.context))?
            .validate_durable_chain()
            .await?;
        anyhow::ensure!(
            self.context.execution_refs().is_empty(),
            "declined collection has execution authority"
        );
        let source_id = self.source.source_event_id().context("collection source missing")?;
        let source = self
            .service
            .memory
            .find_owned_message_event(self.source.workspace_id(), self.source.owner_id(), source_id)
            .await?
            .context("collection source is not durable")?;
        anyhow::ensure!(
            source.role == "user"
                && source.event_type == "message.created"
                && source.session_key.as_deref() == Some(self.source.session_key())
                && source.run_id.as_deref() == Some(self.source.run_id())
                && source.config_generation_id == Some(self.source.config_generation_id())
                && source.config_source_revision.as_deref() == Some(self.source.config_source_revision())
                && crate::runtime::turn_source::subject_task_id(source.subject.as_ref()) == self.source.task_id()
                && self
                    .context
                    .source_actor_matches(source.channel.as_deref(), source.sender.as_deref())
                && crate::runtime::turn_source::recorded_input_matches(
                    self.source.surface(),
                    &source,
                    self.source.routing_input()
                ),
            "declined collection source differs from admitted input"
        );
        let payload = serde_json::json!({
            "version": 1,
            "decision_id": self.context.decision_id().to_string(),
            "source_event_id": source.event_id,
            "capability": {
                "id": self.capability.id.as_str(),
                "schema": hex::encode(self.capability.fingerprint.0),
                "generation": hex::encode(self.capability.generation.0),
            },
            "reply_sha256": hex::encode(Sha256::digest(DECLINED_REPLY.as_bytes())),
        });
        let owner = crate::runtime::turn_cleanup::current_tracker().context("collection cleanup owner missing")?;
        let ticket = owner.register(cancellation.clone())?;
        tokio::spawn(async move {
            let result = persist_decline(&self, source, payload).await;
            ticket.finish(result.is_ok());
            result
        })
        .await
        .context("collection decline persistence task failed")?
    }
}

async fn persist_decline(
    lease: &ApprovalCollectionLease,
    source: MessageEvent,
    payload: serde_json::Value,
) -> anyhow::Result<()> {
    let fabric = MemoryFabric::new(Arc::clone(&lease.service.memory), &source.workspace_id);
    let event = fabric
        .record_runtime_event_idempotent(
            super::super::scope_from_source(&source),
            "approval.collection.declined",
            "argument collection declined; no operation proposed",
            Some(serde_json::to_string(&payload)?),
            format!("decision-approval:{}:declined", lease.context.decision_id()),
        )
        .await?;
    let durable = lease
        .service
        .memory
        .find_owned_message_event(&source.workspace_id, lease.source.owner_id(), &event.event_id)
        .await?
        .context("collection decline receipt is not durable")?;
    anyhow::ensure!(
        durable.id > 0
            && durable.event_type == "approval.collection.declined"
            && durable.session_key == source.session_key
            && durable.run_id == source.run_id
            && serde_json::to_value(&event)? == serde_json::to_value(&durable)?
            && serde_json::from_str::<serde_json::Value>(
                durable.raw_payload_json.as_deref().context("decline payload missing")?
            )? == payload,
        "collection decline receipt conflicts with issued scope"
    );
    lease
        .context
        .collected_approval_result
        .set(Box::new(super::super::CollectedApprovalResult { event: durable }))
        .map_err(|_| anyhow::anyhow!("collection result already installed"))
}
