//! Process-local source authority for one background task, not a durable grant.
use super::{ChildInputReceipt, ChildProviderBinding};
use crate::runtime::turn_decision::OrdinaryChildPublication;
use crate::runtime::turn_source::OwnedTurnSourceProof;
use crate::tools::ToolExecutionContext;
use anyhow::Context;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) struct OrdinaryTaskAuthority {
    id: uuid::Uuid,
    publication: OrdinaryChildPublication,
    binding: Arc<ChildProviderBinding>,
    proof: Arc<OwnedTurnSourceProof>,
    execution: ToolExecutionContext,
    cancellation: CancellationToken,
    scope: crate::memory::MessageEventScope,
    sequence_claimed: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for OrdinaryTaskAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrdinaryTaskAuthority").finish_non_exhaustive()
    }
}

/// The task driver owns this non-cloneable guard through terminal settlement.
/// Abort, panic and early return revoke every retained authority reference.
#[derive(Debug)]
pub(crate) struct OrdinaryTaskLifetime(CancellationToken);

impl Drop for OrdinaryTaskLifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl ChildProviderBinding {
    pub(crate) fn issue_ordinary_task(
        self: &Arc<Self>,
        caller: &crate::tools::execution::ChildCallContext,
        scope: &crate::memory::MessageEventScope,
        input: &str,
        receipt: Option<&ChildInputReceipt>,
        cancellation: &CancellationToken,
    ) -> anyhow::Result<(Arc<OrdinaryTaskAuthority>, OrdinaryTaskLifetime)> {
        caller.verify_tool("sessions_spawn")?;
        anyhow::ensure!(
            scope.source == crate::memory::MessageEventSource::SessionsSpawn,
            "ordinary task requires a spawn source"
        );
        let publication = caller
            .context()
            .ordinary_child_publication
            .as_ref()
            .context("ordinary task parent publication missing")?;
        publication.verify_caller(caller.context())?;
        let mut bound = scope.clone();
        self.bind_ordinary_input_scope(
            &self.provider(),
            &caller.context().envelope.workspace_id,
            &mut bound,
            Some(caller),
        )?;
        let (mut proof, execution) = self.ordinary_execution_source(&self.provider(), caller, scope, input, receipt)?;
        let id = uuid::Uuid::now_v7();
        Arc::get_mut(&mut proof)
            .context("ordinary task source already shared")?
            .bind_ordinary_task(id)?;
        let cancellation = cancellation.child_token();
        anyhow::ensure!(
            !cancellation.is_cancelled(),
            "ordinary task was cancelled before issuance"
        );
        publication.verify_caller(caller.context())?;
        let authority = Arc::new(OrdinaryTaskAuthority {
            id,
            publication: publication.clone(),
            binding: Arc::clone(self),
            proof,
            execution,
            cancellation: cancellation.clone(),
            scope: bound,
            sequence_claimed: std::sync::atomic::AtomicBool::new(false),
        });
        Ok((authority, OrdinaryTaskLifetime(cancellation)))
    }
}

impl OrdinaryTaskAuthority {
    pub(crate) const fn logical_scope(&self) -> &crate::memory::MessageEventScope {
        &self.scope
    }

    pub(crate) fn verify(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.cancellation.is_cancelled(), "ordinary task authority is closed");
        self.publication.verify_task_scope()?;
        anyhow::ensure!(
            !self.cancellation.is_cancelled(),
            "ordinary task authority closed during verification"
        );
        Ok(())
    }

    pub(crate) fn verify_source(&self, proof: &dyn crate::runtime::turn_source::TurnSourceProof) -> anyhow::Result<()> {
        self.verify()?;
        anyhow::ensure!(
            proof.ordinary_task_authority_id() == Some(self.id)
                && proof.owner_isolated()
                    == crate::runtime::turn_source::TurnSourceProof::owner_isolated(self.proof.as_ref()),
            "ordinary source belongs to another task authority"
        );
        Ok(())
    }

    pub(crate) fn source(&self) -> anyhow::Result<(&Arc<OwnedTurnSourceProof>, &ToolExecutionContext)> {
        self.verify()?;
        Ok((&self.proof, &self.execution))
    }

    pub(crate) const fn binding(&self) -> &Arc<ChildProviderBinding> {
        &self.binding
    }

    pub(crate) fn allowed_tools(&self) -> Option<std::collections::BTreeSet<String>> {
        let agent = self
            .scope
            .agent_id
            .as_deref()
            .and_then(|name| self.publication.generation().effective.agents.get(name))?;
        if agent.allowed_tools.is_empty() || agent.allowed_tools.iter().any(|name| name.trim() == "*") {
            return None;
        }
        let mut allowed: std::collections::BTreeSet<_> = agent
            .allowed_tools
            .iter()
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .collect();
        let history = crate::tools::TRANSCRIPT_HISTORY_LOOKUP_TOOL_NAME;
        if self.publication.allows_tool(history) {
            allowed.insert(history.to_owned());
        }
        Some(allowed)
    }

    pub(crate) const fn publication(&self) -> &OrdinaryChildPublication {
        &self.publication
    }
}

/// A task has one serial source sequence, including steering segments.
pub(crate) struct OrdinaryTaskSegments {
    authority: Arc<OrdinaryTaskAuthority>,
    next: u64,
    previous_input: Option<crate::runtime::child_source::SegmentPredecessor>,
}

impl OrdinaryTaskAuthority {
    pub(crate) fn segments(self: &Arc<Self>) -> anyhow::Result<OrdinaryTaskSegments> {
        self.verify()?;
        anyhow::ensure!(
            self.sequence_claimed
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire
                )
                .is_ok(),
            "ordinary task source sequence already claimed"
        );
        Ok(OrdinaryTaskSegments {
            authority: Arc::clone(self),
            next: 0,
            previous_input: None,
        })
    }
}

impl OrdinaryTaskSegments {
    pub(crate) async fn record(
        &mut self,
        input: &str,
    ) -> anyhow::Result<(Arc<OwnedTurnSourceProof>, ToolExecutionContext)> {
        use crate::runtime::turn_source::TurnSourceProof;
        self.authority.verify()?;
        anyhow::ensure!(
            self.next != 0 || input == self.authority.proof.routing_input(),
            "ordinary task first segment differs from its issued input"
        );
        let following = self
            .next
            .checked_add(1)
            .context("ordinary segment identity exhausted")?;
        let run = format!("child-segment:{}:{}", self.authority.id, self.next);
        let mut scope = self.authority.scope.clone();
        scope.run_id = Some(run.clone());
        scope.subject = Some(crate::memory::MessageEventSubject::Task(
            self.authority.proof.run_id().to_owned(),
        ));
        let mut receipt = super::record_child_input(
            &self.authority.publication.source_fabric(),
            scope,
            input,
            Some(format!("{run}:input")),
            Some(
                serde_json::json!({
                    "task_run_id": self.authority.proof.run_id(),
                    "segment_index": self.next,
                    "previous_segment_input_event_id": self.previous_input.as_ref().map(crate::runtime::child_source::SegmentPredecessor::event_id),
                })
                .to_string(),
            ),
            None,
        )
        .await?;
        self.authority.verify()?;
        let next_previous = receipt
            .as_mut()
            .map(|current| {
                crate::runtime::child_source::ChildInputReceipt::advance_segment(current, self.previous_input.as_ref())
            })
            .transpose()?;
        let proof = Arc::new(self.authority.proof.ordinary_task_segment(
            &self.authority,
            &run,
            input,
            receipt.as_deref(),
        )?);
        let mut envelope = self.authority.execution.envelope.clone();
        envelope.run_id = Some(run);
        envelope.task_id = Some(self.authority.proof.run_id().to_owned());
        envelope.source_message_event_id = proof.source_event_id().map(str::to_owned);
        let execution = ToolExecutionContext::new(envelope, proof.policy_chat_type());
        self.next = following;
        self.previous_input = next_previous;
        Ok((proof, execution))
    }
}
