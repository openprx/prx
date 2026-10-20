//! Source-bound logical task completion; model segments settle independently.
#[cfg(test)]
use super::Memory;
use super::{
    Arc, Context, MemoryFabric, MessageEvent, MessageEventScope, TurnTerminalDraft, TurnTerminalPayload,
    TurnTerminalReceipt, TurnTerminalStatus, ensure_runtime_authority, redact_persisted_json, redact_persisted_text,
    terminal_semantic_scope,
};

pub(crate) struct LogicalTaskTerminalRef {
    fabric: MemoryFabric,
    scope: MessageEventScope,
}

impl LogicalTaskTerminalRef {
    pub(crate) fn granted(
        grant: &crate::runtime::child_source::IssuedChildGrant,
        input: &crate::runtime::child_source::ChildInputReceipt,
        scope: &MessageEventScope,
    ) -> anyhow::Result<Self> {
        grant.verify_receipt(input)?;
        input.verify_backend(&grant.source_fabric().memory())?;
        verify_event_scope(input.event(), scope)?;
        Self::issued(grant.source_fabric().clone(), scope.clone())
    }

    pub(crate) fn ordinary(
        authority: &crate::runtime::child_source::ordinary_task::OrdinaryTaskAuthority,
    ) -> anyhow::Result<Self> {
        authority.verify()?;
        Self::issued(
            authority.publication().source_fabric(),
            authority.logical_scope().clone(),
        )
    }

    pub(crate) fn operator(
        authority: &crate::runtime::operator_spawn::task::OperatorTaskAuthority,
    ) -> anyhow::Result<Self> {
        authority.verify()?;
        Self::issued(authority.fabric().clone(), authority.logical_scope().clone())
    }

    pub(crate) fn ordinary_process(
        authority: &crate::runtime::child_source::ordinary_process::OrdinaryProcessAuthority,
    ) -> anyhow::Result<Self> {
        authority.verify()?;
        Self::issued(authority.source_fabric().clone(), authority.logical_scope())
    }

    pub(crate) fn operator_process(
        authority: &crate::runtime::child_source::operator_process::OperatorProcessAuthority,
    ) -> anyhow::Result<Self> {
        authority.verify()?;
        Self::issued(authority.source_fabric().clone(), authority.logical_scope())
    }

    fn issued(fabric: MemoryFabric, scope: MessageEventScope) -> anyhow::Result<Self> {
        anyhow::ensure!(
            scope.run_id.as_deref().is_some_and(|id| !id.is_empty())
                && scope.owner_id.as_deref().is_some_and(|id| !id.is_empty())
                && scope.session_key.as_deref().is_some_and(|id| !id.is_empty())
                && matches!(
                    scope.source,
                    crate::memory::MessageEventSource::SessionsSpawn | crate::memory::MessageEventSource::SessionWorker
                ),
            "logical task completion requires its issued task identity"
        );
        Ok(Self { fabric, scope })
    }

    pub(crate) fn validate(&self, fabric: &MemoryFabric, draft: &TurnTerminalDraft) -> anyhow::Result<()> {
        anyhow::ensure!(
            Arc::ptr_eq(&self.fabric.memory(), &fabric.memory())
                && self.fabric.workspace_id() == fabric.workspace_id()
                && Some(draft.terminal_id.as_str()) == self.scope.run_id.as_deref()
                && terminal_semantic_scope(&draft.scope) == terminal_semantic_scope(&self.scope)
                && draft
                    .history_scope
                    .as_ref()
                    .is_none_or(|scope| terminal_semantic_scope(scope) == terminal_semantic_scope(&self.scope))
                && draft.decision.is_none()
                && draft.provider_outcome.is_none(),
            "logical completion differs from issued source or attempts model settlement"
        );
        Ok(())
    }

    pub(crate) async fn finalize(
        &self,
        fabric: &MemoryFabric,
        mut draft: TurnTerminalDraft,
    ) -> anyhow::Result<TurnTerminalReceipt> {
        self.validate(fabric, &draft)?;
        if let Some(history) = draft.history.as_mut() {
            history.assistant_content = redact_persisted_text(&history.assistant_content);
        }
        ensure_runtime_authority(&self.scope, "logical-terminal-start").await?;
        let assistant_event = if let Some(history) = draft
            .history
            .as_ref()
            .filter(|_| draft.status == TurnTerminalStatus::Completed)
        {
            let event = fabric
                .record_assistant_message_idempotent(
                    self.scope.clone(),
                    history.assistant_content.clone(),
                    format!("turn:{}:assistant", draft.terminal_id),
                )
                .await?;
            verify_event_scope(&event, &self.scope)?;
            anyhow::ensure!(
                event.workspace_id == fabric.workspace_id()
                    && event.role == "assistant"
                    && event.event_type == "message.created"
                    && event.content == history.assistant_content,
                "logical assistant retry conflicts with its original projection"
            );
            Some(event)
        } else {
            None
        };
        let payload = TurnTerminalPayload {
            terminal_id: &draft.terminal_id,
            decision_id: None,
            model_catalog_revision: None,
            model_catalog_content_sha256: None,
            status: draft.status,
            history: &draft.history,
            history_scope: draft.history_scope.as_ref().map(terminal_semantic_scope),
            execution_trace: None,
            assistant_event_id: assistant_event.as_ref().map(|event| event.event_id.as_str()),
            provider_outcome: &None,
            prepared_parameters: &Vec::new(),
            served_auxiliary: None,
            usage_settlement: &None,
            cost_settlement: &None,
            telemetry: &draft.telemetry,
            delivery_intent: &draft.delivery_intent,
            attempt_id: self.scope.attempt_id.as_deref(),
            lease_epoch: self.scope.lease_epoch,
        };
        let expected = redact_persisted_json(serde_json::to_value(payload)?);
        ensure_runtime_authority(&self.scope, "logical-terminal-marker").await?;
        let terminal_event = fabric
            .record_runtime_event_idempotent(
                self.scope.clone(),
                "turn.finalized",
                format!("terminal_id={} status={:?}", draft.terminal_id, draft.status),
                Some(serde_json::to_string(&expected)?),
                format!("turn:{}:final", draft.terminal_id),
            )
            .await?;
        verify_event_scope(&terminal_event, &self.scope)?;
        anyhow::ensure!(
            terminal_event.workspace_id == fabric.workspace_id()
                && terminal_event.role == "event"
                && terminal_event.event_type == "turn.finalized",
            "logical terminal event is not the expected marker"
        );
        let actual: serde_json::Value = serde_json::from_str(
            terminal_event
                .raw_payload_json
                .as_deref()
                .context("logical terminal payload missing")?,
        )?;
        anyhow::ensure!(
            actual == expected,
            "logical terminal retry conflicts with its original proposal"
        );
        Ok(TurnTerminalReceipt {
            terminal_id: draft.terminal_id,
            terminal_event,
            assistant_event,
            usage_settlement: None,
            cost_settlement: None,
            delivery_intent: draft.delivery_intent,
        })
    }
}

fn verify_event_scope(event: &MessageEvent, scope: &MessageEventScope) -> anyhow::Result<()> {
    let actual = serde_json::to_value(event)?;
    let expected = terminal_semantic_scope(scope);
    for (key, value) in expected.as_object().context("logical scope is not an object")? {
        anyhow::ensure!(
            actual.get(key).unwrap_or(&serde_json::Value::Null) == value,
            "logical event scope differs from issued task"
        );
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::super::{TurnDeliveryIntent, TurnHistoryProjection, TurnTerminalTelemetry};
    use super::{
        Arc, LogicalTaskTerminalRef, Memory, MemoryFabric, MessageEventScope, TurnTerminalDraft, TurnTerminalStatus,
    };

    #[tokio::test]
    async fn logical_writer_rejects_foreign_scope_and_preserves_retry_identity() {
        let workspace = tempfile::tempdir().expect("workspace");
        let memory: Arc<dyn Memory> = Arc::new(crate::memory::SqliteMemory::new(workspace.path()).expect("SQLite"));
        let fabric = MemoryFabric::new(memory, workspace.path().to_string_lossy());
        let scope = MessageEventScope::new("sessions_spawn", crate::memory::MemoryVisibility::Session)
            .with_owner_id("prx_owner")
            .with_session_key("prx_session")
            .with_run_id("prx_task");
        // Writer-level fixture; production issuance requires one of the typed
        // grant/ordinary/operator authorities above, never arbitrary scope.
        let authority = LogicalTaskTerminalRef::issued(fabric.clone(), scope.clone()).expect("writer fixture");
        let now = chrono::Utc::now();
        let draft = TurnTerminalDraft {
            decision: None,
            terminal_id: "prx_task".to_owned(),
            scope,
            status: TurnTerminalStatus::Completed,
            history: Some(TurnHistoryProjection {
                assistant_content: "PRX task result".to_owned(),
                history_commit_len: 2,
            }),
            history_scope: None,
            provider_outcome: None,
            telemetry: TurnTerminalTelemetry {
                summary: "task finished".to_owned(),
                started_at: now,
                finished_at: now,
            },
            delivery_intent: TurnDeliveryIntent::None,
        };
        let mut foreign = draft.clone();
        foreign.scope.owner_id = Some("prx_other_owner".to_owned());
        assert!(authority.finalize(&fabric, foreign).await.is_err());
        assert!(
            fabric
                .memory()
                .find_message_event_by_idempotency_key(fabric.workspace_id(), "turn:prx_task:final")
                .await
                .expect("lookup")
                .is_none()
        );
        let first = authority
            .finalize(&fabric, draft.clone())
            .await
            .expect("logical completion");
        let again = authority.finalize(&fabric, draft.clone()).await.expect("exact retry");
        assert_eq!(first.terminal_event.event_id, again.terminal_event.event_id);
        assert_eq!(
            first.assistant_event.as_ref().expect("assistant").event_id,
            again.assistant_event.as_ref().expect("same assistant").event_id
        );
        assert!(first.usage_settlement.is_none() && first.cost_settlement.is_none());
        let mut changed = draft;
        changed.history.as_mut().expect("history").assistant_content = "changed result".to_owned();
        assert!(authority.finalize(&fabric, changed).await.is_err());
        assert!(!workspace.path().join("self/causal_tree/.decision_key").exists());
    }
}
