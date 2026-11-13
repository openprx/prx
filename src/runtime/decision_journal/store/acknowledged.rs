//! Commit and read back an owned decision in one writer-queue operation.
use super::{
    DecisionReplayRecord, DecisionScope, DecisionStoreWriter, RecoveryReadBudget, StoreError, WriterMessage,
    WriterThread, recovery_scope_bounds, recovery_time_left, replay_record_on_connection_until,
};
use std::time::Instant;
use tokio::sync::oneshot;

/// FIFO readback after an admitted chain, on the writer's existing connection.
/// This reads actual scoped rows; a prior queue admission grants no authority.
pub(super) struct ReplayChain {
    scope: DecisionScope,
    ids: Vec<crate::security::decision_id::DecisionId>,
    deadline: Instant,
    done: oneshot::Sender<Result<Vec<Option<DecisionReplayRecord>>, StoreError>>,
}

impl DecisionStoreWriter {
    pub(crate) async fn replay_chain(
        &self,
        scope: DecisionScope,
        ids: Vec<crate::security::decision_id::DecisionId>,
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
    pub(super) fn finish(self, thread: &mut WriterThread) {
        let result = (|| {
            recovery_time_left(self.deadline)?;
            let conn = RecoveryReadBudget::new(&mut thread.conn, self.deadline)?;
            let transaction = conn.unchecked_transaction()?;
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
