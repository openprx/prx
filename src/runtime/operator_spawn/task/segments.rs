//! One serial sequence of actual task inputs, including steering instructions.
use super::{Arc, Context, OperatorTaskAuthority, Ordering, OwnedTurnSourceProof, TurnSourceProof};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::ToolExecutionContext;

pub(crate) struct OperatorTaskSegments {
    authority: Arc<OperatorTaskAuthority>,
    next: u64,
    previous_input: Option<crate::runtime::child_source::SegmentPredecessor>,
}

impl OperatorTaskAuthority {
    pub(crate) fn segments(self: &Arc<Self>) -> anyhow::Result<OperatorTaskSegments> {
        self.verify()?;
        anyhow::ensure!(
            self.sequence_claimed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "operator task input sequence already claimed"
        );
        Ok(OperatorTaskSegments {
            authority: Arc::clone(self),
            next: 0,
            previous_input: None,
        })
    }
}

impl OperatorTaskSegments {
    pub(crate) async fn record(
        &mut self,
        input: &str,
    ) -> anyhow::Result<(Arc<OwnedTurnSourceProof>, ToolExecutionContext)> {
        let authority = &self.authority;
        authority.verify()?;
        anyhow::ensure!(
            self.next != 0 || input == authority.input,
            "operator first segment differs from task input"
        );
        let following = self
            .next
            .checked_add(1)
            .context("operator segment identity exhausted")?;
        let run = format!("operator-segment:{}:{}", authority.id, self.next);
        let mut scope = authority.scope.clone();
        scope.run_id = Some(run.clone());
        scope.subject = Some(crate::memory::MessageEventSubject::Task(authority.run_id.clone()));
        let mut receipt = crate::runtime::child_source::record_child_input(
            authority.fabric(),
            scope.clone(),
            input,
            Some(format!("{run}:input")),
            Some(
                serde_json::json!({"task_run_id": authority.run_id, "segment_index": self.next,
                "previous_segment_input_event_id": self.previous_input.as_ref().map(crate::runtime::child_source::SegmentPredecessor::event_id)})
                .to_string(),
            ),
            None,
        )
        .await?;
        authority.verify()?;
        let next_previous = receipt
            .as_mut()
            .map(|current| {
                crate::runtime::child_source::ChildInputReceipt::advance_segment(current, self.previous_input.as_ref())
            })
            .transpose()?;
        let proof = Arc::new(OwnedTurnSourceProof::operator_task_segment(
            authority,
            &run,
            input,
            receipt.as_deref(),
        )?);
        let mut envelope = RuntimeEnvelope::sessions_spawn(proof.workspace_id(), proof.session_key(), &run);
        envelope.owner_id = Some(proof.owner_id().to_owned());
        envelope.owner_isolated = proof.owner_isolated();
        envelope.task_id = Some(authority.run_id.clone());
        envelope.source_message_event_id = proof.source_event_id().map(str::to_owned);
        envelope.parent_run_id = scope.parent_run_id;
        envelope.agent_id = scope.agent_id;
        envelope.persona_id = scope.persona_id;
        envelope.channel = scope.channel;
        envelope.sender = scope.sender;
        envelope.recipient = scope.recipient;
        envelope.attempt_id = scope.attempt_id;
        envelope.lease_epoch = scope.lease_epoch;
        envelope.config_generation_id = scope.config_generation_id;
        envelope.config_source_revision = scope.config_source_revision;
        let execution = ToolExecutionContext::new(envelope, proof.policy_chat_type());
        self.next = following;
        self.previous_input = next_previous;
        Ok((proof, execution))
    }
}
