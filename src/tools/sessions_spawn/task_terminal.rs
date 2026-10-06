//! In-process retention of the exact terminal proposal by the external task observer.
use crate::agent::terminal::{
    TurnDeliveryIntent, TurnTerminalDraft, TurnTerminalReceipt, TurnTerminalStatus, TurnTerminalTelemetry,
};
use crate::memory::{MemoryFabric, MessageEventScope};
use anyhow::Context;

pub(super) struct TaskTerminalJournal {
    fabric: MemoryFabric,
    scope: MessageEventScope,
    authority: crate::agent::terminal::logical::LogicalTaskTerminalRef,
    require_durable: bool,
    segment_usage: Option<std::sync::Arc<crate::runtime::turn_terminal::TerminalUsageCollector>>,
    started_at: chrono::DateTime<chrono::Utc>,
    state: parking_lot::Mutex<State>,
}

#[derive(Default)]
struct State {
    pending: Option<TurnTerminalDraft>,
    receipt: Option<TurnTerminalReceipt>,
}

pub(super) struct CommittedTaskTerminal {
    pub receipt: TurnTerminalReceipt,
    pub status: TurnTerminalStatus,
    pub summary: String,
    pub segment_usage: Vec<crate::llm::route_decision::MeteredTokenUsageRecord>,
}

impl CommittedTaskTerminal {
    /// Validate the complete merge before changing the registry. Repeated
    /// settlements are idempotent only when all recorded usage agrees.
    pub(super) fn reconcile_run(self, run: &mut super::SubAgentRun) -> anyhow::Result<TurnTerminalStatus> {
        use std::collections::BTreeMap;
        let mut known = BTreeMap::new();
        for usage in &run.token_usage_records {
            if let Some(id) = usage.settlement_id.as_deref() {
                if let Some(previous) = known.insert(id, usage) {
                    anyhow::ensure!(
                        previous == usage,
                        "task registry contains conflicting usage settlements"
                    );
                }
            }
        }
        let mut additions = Vec::new();
        for usage in self.segment_usage.iter().chain(self.receipt.usage_settlement.iter()) {
            let id = usage
                .settlement_id
                .as_deref()
                .context("recovered task usage has no settlement id")?;
            if let Some(previous) = known.get(id) {
                anyhow::ensure!(*previous == usage, "recovered task usage conflicts with its settlement");
            } else {
                known.insert(id, usage);
                additions.push(usage.clone());
            }
        }
        let status_matches = matches!(
            (&run.status, self.status),
            (
                super::SubAgentStatus::Completed(_),
                TurnTerminalStatus::Completed | TurnTerminalStatus::Silent
            ) | (
                super::SubAgentStatus::Failed(_),
                TurnTerminalStatus::Failed | TurnTerminalStatus::Cancelled
            )
        );
        if !status_matches {
            run.status = match self.status {
                TurnTerminalStatus::Completed | TurnTerminalStatus::Silent => {
                    super::SubAgentStatus::Completed(self.summary)
                }
                _ => super::SubAgentStatus::Failed(self.summary),
            };
        }
        run.token_usage_records.extend(additions);
        run.finished_at.get_or_insert_with(chrono::Utc::now);
        run.steer_tx = None;
        Ok(self.status)
    }

    fn from_receipt(receipt: TurnTerminalReceipt, expected: TurnTerminalStatus) -> anyhow::Result<Self> {
        let payload: serde_json::Value = serde_json::from_str(
            receipt
                .terminal_event
                .raw_payload_json
                .as_deref()
                .context("task terminal payload missing")?,
        )?;
        anyhow::ensure!(
            payload.get("terminal_id").and_then(serde_json::Value::as_str) == Some(receipt.terminal_id.as_str()),
            "task terminal payload identity differs from receipt"
        );
        let status: TurnTerminalStatus =
            serde_json::from_value(payload.get("status").cloned().context("task terminal status missing")?)?;
        anyhow::ensure!(status == expected, "task terminal status differs from its proposal");
        let summary = payload
            .get("telemetry")
            .and_then(|value| value.get("summary"))
            .and_then(serde_json::Value::as_str)
            .context("task terminal summary missing")?
            .to_owned();
        Ok(Self {
            receipt,
            status,
            summary,
            segment_usage: Vec::new(),
        })
    }
}

impl TaskTerminalJournal {
    pub(super) fn new(
        fabric: MemoryFabric,
        scope: MessageEventScope,
        authority: crate::agent::terminal::logical::LogicalTaskTerminalRef,
        require_durable: bool,
        segment_usage: Option<std::sync::Arc<crate::runtime::turn_terminal::TerminalUsageCollector>>,
    ) -> Self {
        Self {
            fabric,
            scope,
            authority,
            require_durable,
            segment_usage,
            started_at: chrono::Utc::now(),
            state: parking_lot::Mutex::new(State::default()),
        }
    }

    pub(super) async fn commit(&self, proposal: TurnTerminalDraft) -> anyhow::Result<TurnTerminalReceipt> {
        self.authority.validate(&self.fabric, &proposal)?;
        anyhow::ensure!(
            proposal.decision.is_none()
                && Some(proposal.terminal_id.as_str()) == self.scope.run_id.as_deref()
                && proposal.scope.source == self.scope.source
                && proposal.scope.owner_id == self.scope.owner_id
                && proposal.scope.session_key == self.scope.session_key
                && proposal.scope.run_id == self.scope.run_id
                && proposal.scope.parent_run_id == self.scope.parent_run_id
                && proposal.scope.config_generation_id == self.scope.config_generation_id
                && proposal.scope.config_source_revision == self.scope.config_source_revision,
            "logical task terminal differs from its admitted scope"
        );
        {
            let mut state = self.state.lock();
            anyhow::ensure!(state.pending.is_none(), "logical task terminal proposal already exists");
            state.pending = Some(proposal);
        }
        Ok(self.flush().await?.receipt)
    }

    /// Called only after the driver has joined. An interrupted write retries
    /// the original proposal; an earlier abort creates one cancellation/failure
    /// proposal, never a speculative successful result or fabricated usage.
    pub(super) async fn recover(
        &self,
        interruption: Option<(TurnTerminalStatus, String)>,
    ) -> anyhow::Result<CommittedTaskTerminal> {
        {
            let mut state = self.state.lock();
            if state.pending.is_none() {
                let (status, summary) = interruption.context("task ended without a terminal proposal")?;
                state.pending = Some(TurnTerminalDraft {
                    decision: None,
                    terminal_id: self.scope.run_id.clone().context("task terminal run id missing")?,
                    scope: self.scope.clone(),
                    status,
                    history: None,
                    history_scope: None,
                    provider_outcome: None,
                    telemetry: TurnTerminalTelemetry {
                        summary,
                        started_at: self.started_at,
                        finished_at: chrono::Utc::now(),
                    },
                    delivery_intent: TurnDeliveryIntent::Suppress {
                        reason: "task observer recovered an interrupted run; provider usage may be incomplete"
                            .to_owned(),
                    },
                });
            }
        }
        let mut committed = self.flush().await?;
        if let Some(collector) = &self.segment_usage {
            committed.segment_usage = collector.snapshot();
        }
        Ok(committed)
    }

    async fn flush(&self) -> anyhow::Result<CommittedTaskTerminal> {
        let proposal = {
            let state = self.state.lock();
            let pending = state.pending.as_ref().context("task terminal proposal missing")?;
            if let Some(receipt) = &state.receipt {
                return CommittedTaskTerminal::from_receipt(receipt.clone(), pending.status);
            }
            pending.clone()
        };
        let status = proposal.status;
        let receipt = self.authority.finalize(&self.fabric, proposal).await?;
        anyhow::ensure!(
            !self.require_durable || receipt.terminal_event.id > 0,
            "granted task terminal must be durable"
        );
        if receipt.terminal_event.id > 0 {
            let event = &receipt.terminal_event;
            let expected_key = format!("turn:{}:final", receipt.terminal_id);
            anyhow::ensure!(
                event.workspace_id == self.fabric.workspace_id()
                    && event.owner_id == self.scope.owner_id
                    && event.run_id == self.scope.run_id
                    && event.session_key == self.scope.session_key
                    && event.source == self.scope.source
                    && event.event_type == "turn.finalized"
                    && event.idempotency_key.as_deref() == Some(expected_key.as_str()),
                "task terminal receipt differs from its owner and run"
            );
            let owner = self.scope.owner_id.as_deref().context("task terminal owner missing")?;
            let stored = self
                .fabric
                .memory()
                .find_owned_message_event(self.fabric.workspace_id(), owner, &event.event_id)
                .await?
                .context("task terminal receipt is not durable")?;
            anyhow::ensure!(
                serde_json::to_value(&stored)? == serde_json::to_value(event)?,
                "task terminal receipt differs from durable record"
            );
        }
        self.state.lock().receipt = Some(receipt.clone());
        CommittedTaskTerminal::from_receipt(receipt, status)
    }
}
