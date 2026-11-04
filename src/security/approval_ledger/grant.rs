//! Issued grant persistence and durable event receipts.
use super::{ApprovalLedger, GrantScope};
use crate::acl::approval_grant::{ApprovalGrantV2, WitnessKeyring};
use crate::memory::{MemoryFabric, MessageEvent, MessageEventScope};
use anyhow::Context;
use chrono::Utc;

impl ApprovalLedger<'_> {
    pub(super) async fn verify_approval_event_scope(
        &self,
        scope: &MessageEventScope,
        expected: &GrantScope<'_>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.enabled,
            "decision approval storage requires durable enabled service"
        );
        anyhow::ensure!(
            expected.subject.workspace_id == self.workspace.as_str()
                && scope.owner_id.as_deref() == Some(expected.subject.owner_id.as_str())
                && scope.session_key == expected.subject.session_key
                && expected.binding.catalog_generation == self.catalog_generation,
            "decision approval storage scope differs from pinned authority"
        );
        let source_id = scope
            .causation_event_id
            .as_deref()
            .context("approval source event missing")?;
        let source = self
            .memory
            .find_owned_message_event(&expected.subject.workspace_id, &expected.subject.owner_id, source_id)
            .await?
            .context("approval source is not durable")?;
        anyhow::ensure!(
            source.role == "user"
                && source.event_type == "message.created"
                && source.source == scope.source
                && source.channel == scope.channel
                && source.sender == scope.sender
                && source.recipient == scope.recipient
                && source.session_key == scope.session_key
                && source.run_id == scope.run_id
                && source.visibility == scope.visibility
                && source.config_generation_id == scope.config_generation_id
                && source.config_source_revision == scope.config_source_revision,
            "decision approval event scope differs from original input"
        );
        self.port.check_live_parity()?;
        Ok(())
    }

    /// Persist the exact signed grant; a retry cannot replace its subject,
    /// decision, call, expiry or operation under the same grant id.
    pub(crate) async fn store_decision_grant(
        &self,
        scope: MessageEventScope,
        grant: &ApprovalGrantV2,
        expected: &GrantScope<'_>,
        keyring: &WitnessKeyring,
    ) -> anyhow::Result<()> {
        grant.verify_for_decision(
            keyring,
            expected.subject,
            expected.binding,
            expected.op_id,
            expected.risk,
            Utc::now(),
        )?;
        self.verify_approval_event_scope(&scope, expected).await?;
        let fabric = MemoryFabric::new(self.memory.clone(), expected.subject.workspace_id.clone());
        let idempotency_key = format!("decision-approval:{}:issued", grant.grant_id);
        let event = fabric
            .record_runtime_event_idempotent(
                scope.clone(),
                "approval.decision.issued",
                "decision approval recorded",
                Some(serde_json::to_string(grant)?),
                &idempotency_key,
            )
            .await?;
        let durable = self.approval_receipt(&event, &scope, &idempotency_key).await?;
        let stored: ApprovalGrantV2 = serde_json::from_str(
            durable
                .raw_payload_json
                .as_deref()
                .context("stored decision approval payload missing")?,
        )?;
        anyhow::ensure!(stored == *grant, "stored decision approval conflicts with signed grant");
        Ok(())
    }

    pub(super) async fn approval_receipt(
        &self,
        event: &MessageEvent,
        scope: &MessageEventScope,
        idempotency_key: &str,
    ) -> anyhow::Result<MessageEvent> {
        let owner = scope.owner_id.as_deref().context("approval receipt owner missing")?;
        let workspace = self.workspace.as_str();
        let durable = self
            .memory
            .find_owned_message_event(&workspace, owner, &event.event_id)
            .await?
            .context("decision approval receipt is not durable")?;
        anyhow::ensure!(
            durable.id > 0
                && durable.idempotency_key.as_deref() == Some(idempotency_key)
                && durable.owner_id == scope.owner_id
                && durable.session_key == scope.session_key
                && durable.run_id == scope.run_id
                && durable.source == scope.source
                && durable.causation_event_id == scope.causation_event_id
                && serde_json::to_value(&durable)? == serde_json::to_value(event)?,
            "decision approval receipt differs from requested scope"
        );
        Ok(durable)
    }
}
