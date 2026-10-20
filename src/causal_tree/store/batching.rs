//! Commit ready ordinary chains together, isolating each chain's conflicts.
use super::{
    CausalEvent, CausalEventFields, CausalEventKind, Connection, DecisionRecord, IncompleteReason, Inserted,
    PRUNE_EVERY, StoreError, WriterMessage, WriterThread, emit_incomplete, insert_decision, lineage_of,
};

const MAX_READY_CHAINS: usize = 32;

type ChainResult = Result<Option<Vec<Inserted>>, StoreError>;

impl WriterThread {
    pub(super) fn write_ready_decisions(&mut self, first: Box<[DecisionRecord]>, pending: &mut Option<WriterMessage>) {
        let mut record_count = first.len();
        // Leave the normal single-chain path allocation-free.
        let mut batches = Vec::new();
        // End at the same original-chain boundary that would trigger pruning.
        let before_prune = usize::try_from(PRUNE_EVERY.saturating_sub(self.writes_since_prune)).unwrap_or(0);
        while batches.len() < MAX_READY_CHAINS - 1 && record_count < before_prune {
            match self.commands.try_recv() {
                Ok(WriterMessage::DecisionBatch(records)) => {
                    record_count = record_count.saturating_add(records.len());
                    batches.push(records);
                }
                Ok(boundary) => {
                    *pending = Some(boundary);
                    break;
                }
                Err(_) => break,
            }
        }
        if batches.is_empty() {
            self.write_decisions(&first);
            return;
        }
        batches.insert(0, first);
        match insert_ready(&mut self.conn, &batches) {
            Ok(results) => {
                // Nothing reports DecisionCommitted until the outer transaction
                // has committed, including every successfully released savepoint.
                for (records, result) in batches.iter().zip(results) {
                    match result {
                        Ok(Some(inserted)) => {
                            for (record, status) in records.iter().zip(inserted) {
                                if status == Inserted::Recorded {
                                    self.events.emit(CausalEvent {
                                        kind: CausalEventKind::DecisionCommitted,
                                        lineage: lineage_of(record),
                                        fields: CausalEventFields::default(),
                                    });
                                }
                            }
                        }
                        Ok(None) => {
                            tracing::warn!(
                                "a conflicting decision chain was refused; no new rows from that chain were kept"
                            );
                            for record in records {
                                emit_incomplete(self.events.as_ref(), lineage_of(record), IncompleteReason::Conflict);
                            }
                        }
                        Err(error) => {
                            tracing::warn!("decision store chain write failed: {error}");
                            for record in records {
                                emit_incomplete(
                                    self.events.as_ref(),
                                    lineage_of(record),
                                    IncompleteReason::StoreFailed,
                                );
                            }
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!("decision store ready batch failed: {error}");
                for records in &batches {
                    for record in records {
                        emit_incomplete(self.events.as_ref(), lineage_of(record), IncompleteReason::StoreFailed);
                    }
                }
            }
        }
        self.finish_decision_write(record_count);
    }
}

fn insert_ready(conn: &mut Connection, batches: &[Box<[DecisionRecord]>]) -> Result<Vec<ChainResult>, StoreError> {
    let mut transaction = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut results = Vec::with_capacity(batches.len());
    for records in batches {
        let mut savepoint = transaction.savepoint()?;
        let result = (|| {
            let mut inserted = Vec::with_capacity(records.len());
            for record in records {
                match insert_decision(&savepoint, record)? {
                    Inserted::Conflict => return Ok(None),
                    status => inserted.push(status),
                }
            }
            Ok(Some(inserted))
        })();
        if !matches!(result, Ok(Some(_))) {
            // Rollback itself must succeed before any neighbor can be accepted.
            savepoint.rollback()?;
        }
        savepoint.commit()?;
        results.push(result);
    }
    transaction.commit()?;
    Ok(results)
}

#[cfg(test)]
mod tests;
