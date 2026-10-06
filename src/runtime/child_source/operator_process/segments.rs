use super::{Arc, Context, OperatorProcessAuthority};
use crate::runtime::turn_source::{OwnedTurnSourceProof, TurnSourceProof};
use crate::tools::ToolExecutionContext;

/// One non-cloneable sequence per authenticated worker lifetime.
pub(crate) struct OperatorProcessSegments {
    authority: Arc<OperatorProcessAuthority>,
    initial_proof: OwnedTurnSourceProof,
    next: u64,
    previous_input: Option<crate::runtime::child_source::SegmentPredecessor>,
}

impl OperatorProcessAuthority {
    pub(crate) fn segments(self: &Arc<Self>) -> anyhow::Result<OperatorProcessSegments> {
        let initial_proof = self.source_proof()?;
        anyhow::ensure!(
            self.sequence_claimed
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok(),
            "operator worker source sequence already claimed"
        );
        self.verify()?;
        Ok(OperatorProcessSegments {
            authority: Arc::clone(self),
            initial_proof,
            next: 0,
            previous_input: None,
        })
    }
}

impl OperatorProcessSegments {
    pub(crate) const fn recorded_count(&self) -> u64 {
        self.next
    }

    pub(crate) async fn record(
        &mut self,
        input: &str,
    ) -> anyhow::Result<(Arc<OwnedTurnSourceProof>, ToolExecutionContext)> {
        self.authority.verify()?;
        anyhow::ensure!(
            self.next != 0 || input == self.authority.task(),
            "operator worker initial input differs"
        );
        let following = self
            .next
            .checked_add(1)
            .context("operator worker segment identity exhausted")?;
        let run = format!("operator-worker-segment:{}:{}", self.authority.id(), self.next);
        let mut scope = self.authority.initial_scope.clone();
        scope.run_id = Some(run.clone());
        scope.subject = Some(crate::memory::MessageEventSubject::Task(
            self.authority.run_id().to_owned(),
        ));
        let mut receipt = super::super::record_child_input(
            self.authority.source_fabric(),
            scope.clone(),
            input,
            Some(format!("{run}:input")),
            Some(
                serde_json::json!({
                    "task_run_id": self.authority.run_id(),
                    "task_input_event_id": self.authority.input().map(|receipt| receipt.event().event_id.as_str()),
                    "segment_index": self.next,
                    "previous_segment_input_event_id": self.previous_input.as_ref().map(crate::runtime::child_source::SegmentPredecessor::event_id),
                })
                .to_string(),
            ),
            None,
        )
        .await?;
        self.authority.verify()?;
        super::journal::OperatorSegmentJournal::new(&self.authority)?
            .admission(self.next, receipt.as_deref())
            .await?;
        let next_previous = receipt
            .as_mut()
            .map(|current| {
                crate::runtime::child_source::ChildInputReceipt::advance_segment(current, self.previous_input.as_ref())
            })
            .transpose()?;
        let proof =
            Arc::new(
                self.initial_proof
                    .operator_process_segment(&self.authority, &run, input, receipt.as_deref())?,
            );
        let mut envelope =
            crate::runtime::envelope::RuntimeEnvelope::session_worker(proof.workspace_id(), proof.session_key(), &run);
        envelope.owner_id = Some(proof.owner_id().to_owned());
        envelope.owner_isolated = proof.owner_isolated();
        envelope.task_id = Some(self.authority.run_id().to_owned());
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
        self.authority.verify_source(proof.as_ref())?;
        self.next = following;
        self.previous_input = next_previous;
        Ok((proof, execution))
    }
}
