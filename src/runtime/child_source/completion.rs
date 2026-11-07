//! Child completion settlement. Admission stays in the gated adapter.
use crate::agent::terminal::VerifiedTerminalReplay;
use crate::causal_tree::outcome::{OutcomeCorrelator, TerminalFacts};
use crate::decision::DecisionKey;
use crate::memory::{MessageEvent, MessageEventSource};
use crate::runtime::child_source::authority_journal::{self, CHILD_OUTCOME_VERSION, ChildOutcomeRow};
use crate::runtime::turn_cleanup::{CleanupTracker, current_tracker};
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Every construction, scope, codec, seal, conflict, SQLite, tracker, join and
/// deadline failure of the child outcome journal. The source is preserved.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ChildOutcomeError {
    #[error("child outcome journal: {0}")]
    Journal(#[source] anyhow::Error),
}

/// Writes the sealed `child_outcome` row after a verified durable terminal.
#[async_trait::async_trait]
pub(crate) trait ChildOutcomeHook: Send + Sync {
    async fn persist(
        &self,
        replay: &VerifiedTerminalReplay,
        terminal: &MessageEvent,
        owner: Option<&Arc<CleanupTracker>>,
        deadline: Option<Instant>,
    ) -> Result<(), ChildOutcomeError>;
}

/// The journal hook of one recorded settlement: its workspace, its decision
/// key and its durable store namespace.
pub(crate) struct JournalChildOutcomeHook {
    workspace: PathBuf,
    key: Arc<DecisionKey>,
    store_workspace: Arc<str>,
}

impl JournalChildOutcomeHook {
    pub(crate) const fn new(workspace: PathBuf, key: Arc<DecisionKey>, store_workspace: Arc<str>) -> Self {
        Self {
            workspace,
            key,
            store_workspace,
        }
    }

    /// Builds the expected row from the verified replay and the persisted marker.
    /// No `child_outcome` lookup takes part in this derivation.
    pub(crate) fn expected_row(
        &self,
        replay: &VerifiedTerminalReplay,
        terminal: &MessageEvent,
    ) -> anyhow::Result<ChildOutcomeRow> {
        let source = replay.source();
        anyhow::ensure!(
            terminal.id > 0
                && terminal.owner_id == source.owner_id
                && terminal.workspace_id == source.workspace_id
                && terminal.run_id == source.run_id,
            "child terminal scope differs from its verified source"
        );
        let run = source
            .run_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("verified child run missing"))?;
        let owner = source
            .owner_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("verified child owner missing"))?;
        anyhow::ensure!(
            replay.parent_decision_id().is_some() == replay.parent_digest().is_some(),
            "child parent id and digest must be both present or both absent"
        );
        let facts = TerminalFacts::from_persisted_marker(terminal, replay.decision(), owner, &terminal.workspace_id)?
            .with_verified_resume(replay);
        let outcome = OutcomeCorrelator::persisted_record(replay.decision(), &facts)?;
        let payload: serde_json::Value = serde_json::from_str(
            terminal
                .raw_payload_json
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("terminal payload missing"))?,
        )?;
        let terminal_id = payload
            .get("terminal_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("terminal payload has no terminal id"))?;
        anyhow::ensure!(
            run == terminal_id && run == outcome.settlement_id(),
            "child outcome identity differs from its terminal"
        );
        Ok(ChildOutcomeRow {
            version: CHILD_OUTCOME_VERSION,
            run: run.to_owned(),
            settlement: outcome.settlement_id().to_owned(),
            terminal_id: terminal_id.to_owned(),
            owner: owner.to_owned(),
            store_workspace: self.store_workspace.to_string(),
            parent_decision: replay.parent_decision_id().map(|id| id.to_string()),
            parent_digest: replay.parent_digest().map(str::to_owned),
            child_decision: replay.decision().decision_id.to_string(),
            child_digest: replay.decision_digest().to_owned(),
            outcome_hash: outcome.journal_hash()?,
        })
    }
}

/// A deadline that has passed fails closed. Physical work stays ticket-owned.
fn ensure_before(deadline: Option<Instant>) -> Result<(), ChildOutcomeError> {
    match deadline {
        Some(deadline) if Instant::now() >= deadline => Err(ChildOutcomeError::Journal(anyhow::anyhow!(
            "child outcome deadline elapsed"
        ))),
        _ => Ok(()),
    }
}

#[async_trait::async_trait]
impl ChildOutcomeHook for JournalChildOutcomeHook {
    async fn persist(
        &self,
        replay: &VerifiedTerminalReplay,
        terminal: &MessageEvent,
        owner: Option<&Arc<CleanupTracker>>,
        deadline: Option<Instant>,
    ) -> Result<(), ChildOutcomeError> {
        // Verified non-child sources never own a child outcome row.
        if !matches!(
            replay.source().source,
            MessageEventSource::Delegate | MessageEventSource::SessionsSpawn | MessageEventSource::SessionWorker
        ) {
            return Ok(());
        }
        let row = self
            .expected_row(replay, terminal)
            .map_err(ChildOutcomeError::Journal)?;
        if deadline.is_some() && owner.is_none() {
            return Err(ChildOutcomeError::Journal(anyhow::anyhow!(
                "child outcome deadline requires an explicit owner"
            )));
        }
        ensure_before(deadline)?;
        let tracker = owner.cloned().or_else(current_tracker);
        let ticket = tracker
            .map(|tracker| tracker.register(CancellationToken::new()))
            .transpose()
            .map_err(ChildOutcomeError::Journal)?;
        let workspace = self.workspace.clone();
        let key = Arc::clone(&self.key);
        // The write runs on a blocking thread and is always awaited, so cancellation
        // cannot drop it midway. The ticket finishes inside the closure.
        let written = crate::runtime::blocking::spawn_blocking(move || {
            let result = authority_journal::persist_child_outcome(&workspace, &key, &row);
            if let Some(ticket) = ticket {
                ticket.finish(result.is_ok());
            }
            result
        })
        .await
        .map_err(|error| ChildOutcomeError::Journal(error.into()))?;
        written?;
        ensure_before(deadline)
    }
}

/// Completion uses the admitted segment's prebound terminal and usage owner.
/// It cannot admit a segment or reconstruct parent authority from a decision row.
pub(crate) struct ChildCompletionPort {
    pub(crate) usage: Arc<crate::runtime::turn_terminal::TerminalUsageCollector>,
}

impl ChildCompletionPort {
    pub(crate) async fn complete(
        &self,
        turn: &crate::runtime::child_turn::ChildSegmentTurn,
        outcome: &crate::agent::turn_contracts::ToolLoopOutcome,
        trace: &crate::agent::turn_contracts::ToolLoopTrace,
    ) -> Result<()> {
        turn.complete(outcome, trace).await.map(|_| ())
    }

    pub(crate) async fn close(
        &self,
        turn: &crate::runtime::child_turn::ChildSegmentTurn,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>> {
        turn.close(cancellation).await
    }

    pub(crate) async fn fail(
        &self,
        turn: &crate::runtime::child_turn::ChildSegmentTurn,
        error: &anyhow::Error,
        cancelled: bool,
    ) -> Result<()> {
        turn.fail(error, cancelled).await
    }
}
