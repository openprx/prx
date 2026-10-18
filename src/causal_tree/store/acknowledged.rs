//! Commit and read back an owned decision in one writer-queue operation.
use super::{
    DecisionRecord, DecisionReplayRecord, DecisionScope, DecisionStoreWriter, IncompleteReason, RecoveryReadBudget,
    StoreError, TrySendError, WriterMessage, WriterThread, emit_incomplete, lineage_of, recovery_scope_bounds,
    recovery_time_left, replay_record_on_connection_until, same_turn_chain,
};
use std::{sync::Arc, time::Instant};
use tokio::sync::oneshot;

pub(super) struct VerifiedDecision {
    record: DecisionRecord,
    scope: DecisionScope,
    deadline: Instant,
    done: oneshot::Sender<Result<Option<DecisionReplayRecord>, StoreError>>,
}

impl DecisionStoreWriter {
    /// The returned row is decoded from SQLite after commit, with the same
    /// scope, digest and bounds as a recovery read. Queue admission is bounded;
    /// expiry never turns a late write into permission to continue the read.
    pub(crate) async fn record_and_replay(
        &self,
        mut record: DecisionRecord,
        scope: DecisionScope,
        deadline: Instant,
    ) -> Result<Option<DecisionReplayRecord>, StoreError> {
        recovery_time_left(deadline)?;
        recovery_scope_bounds(&scope)?;
        if !same_turn_chain(std::slice::from_ref(&record)) {
            emit_incomplete(self.events.as_ref(), lineage_of(&record), IncompleteReason::Conflict);
            return Err(StoreError::RecoveryUnavailable);
        }
        record.lineage.workspace_id = Arc::clone(&self.workspace_id);
        let (done, finished) = oneshot::channel();
        let command = WriterMessage::VerifiedDecision(Box::new(VerifiedDecision {
            record,
            scope,
            deadline,
            done,
        }));
        if let Err(TrySendError::Full(command) | TrySendError::Disconnected(command)) = self.queue.try_send(command) {
            if let WriterMessage::VerifiedDecision(command) = command {
                emit_incomplete(
                    self.events.as_ref(),
                    lineage_of(&command.record),
                    IncompleteReason::StoreDropped,
                );
            }
            return Err(StoreError::RecoveryUnavailable);
        }
        let row = tokio::time::timeout_at(deadline.into(), finished)
            .await
            .map_err(|_| StoreError::RecoveryDeadline)?
            .map_err(|_| StoreError::RecoveryUnavailable)??;
        recovery_time_left(deadline)?;
        Ok(row)
    }
}

impl VerifiedDecision {
    pub(super) fn finish(self, thread: &mut WriterThread) {
        // This keeps the existing transaction, conflict handling, event and
        // retention path. No acknowledgement is based on the supplied record.
        thread.write_decisions(std::slice::from_ref(&self.record));
        let result = (|| {
            recovery_time_left(self.deadline)?;
            let _budget = RecoveryReadBudget::new(&thread.conn, self.deadline)?;
            let transaction = thread.conn.unchecked_transaction()?;
            let row =
                replay_record_on_connection_until(&transaction, &self.scope, &self.record.decision_id, self.deadline)?;
            recovery_time_left(self.deadline)?;
            transaction.commit()?;
            recovery_time_left(self.deadline)?;
            Ok(row)
        })();
        let _ = self.done.send(result);
    }
}

/// FIFO readback after an admitted chain, on the writer's existing connection.
/// This reads actual scoped rows; a prior queue admission grants no authority.
pub(super) struct ReplayChain {
    scope: DecisionScope,
    ids: Vec<crate::causal_tree::ids::DecisionId>,
    deadline: Instant,
    done: oneshot::Sender<Result<Vec<Option<DecisionReplayRecord>>, StoreError>>,
}

impl DecisionStoreWriter {
    pub(crate) async fn replay_chain(
        &self,
        scope: DecisionScope,
        ids: Vec<crate::causal_tree::ids::DecisionId>,
        deadline: Instant,
    ) -> Result<Vec<Option<DecisionReplayRecord>>, StoreError> {
        recovery_time_left(deadline)?;
        recovery_scope_bounds(&scope)?;
        if ids.is_empty() || ids.len() > 5 {
            return Err(StoreError::RecoveryUnavailable);
        }
        let (done, finished) = oneshot::channel();
        self.queue
            .try_send(WriterMessage::ReplayChain(Box::new(ReplayChain {
                scope,
                ids,
                deadline,
                done,
            })))
            .map_err(|_| StoreError::RecoveryUnavailable)?;
        let rows = tokio::time::timeout_at(deadline.into(), finished)
            .await
            .map_err(|_| StoreError::RecoveryDeadline)?
            .map_err(|_| StoreError::RecoveryUnavailable)??;
        recovery_time_left(deadline)?;
        Ok(rows)
    }
}

impl ReplayChain {
    pub(super) fn finish(self, thread: &WriterThread) {
        let result = (|| {
            recovery_time_left(self.deadline)?;
            let _budget = RecoveryReadBudget::new(&thread.conn, self.deadline)?;
            let transaction = thread.conn.unchecked_transaction()?;
            let mut rows = Vec::with_capacity(self.ids.len());
            for id in &self.ids {
                rows.push(replay_record_on_connection_until(
                    &transaction,
                    &self.scope,
                    id,
                    self.deadline,
                )?);
            }
            recovery_time_left(self.deadline)?;
            transaction.commit()?;
            recovery_time_left(self.deadline)?;
            Ok(rows)
        })();
        let _ = self.done.send(result);
    }
}

#[cfg(test)]
mod tests;
