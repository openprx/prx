//! Immutable links from authenticated child terminals to their parent decision.
use super::{
    Arc, CausalLineage, Connection, DecisionId, DecisionScope, DecisionStoreReader, DecisionStoreWriter,
    EncodedOutcome, Enqueue, IncompleteReason, Instant, OptionalExtension, OutcomeRecord, StoreError, WriterMessage,
    WriterThread, emit_incomplete, outcome_lineage, outcome_matches_on_connection, outcome_matches_on_connection_until,
    params, recovery_schema, recovery_scope_bounds, recovery_size_sql, recovery_time_left, replay_record_on_connection,
    replay_record_on_connection_until, store_schema,
};

pub(crate) struct ChildOutcomeLink {
    parent: DecisionId,
    child: DecisionId,
    owner: Arc<str>,
    parent_digest: String,
    child_digest: String,
    outcome: OutcomeRecord,
}

impl ChildOutcomeLink {
    pub(crate) fn from_replay(
        replay: &crate::agent::terminal::VerifiedTerminalReplay,
        outcome: &OutcomeRecord,
    ) -> Option<Self> {
        let parent = replay.parent_decision_id()?;
        if parent == replay.decision().decision_id
            || outcome.decision_id != replay.decision().decision_id
            || replay.source().run_id.as_deref() != Some(outcome.settlement_id.as_str())
        {
            return None;
        }
        Some(Self {
            parent,
            child: outcome.decision_id,
            owner: Arc::from(replay.source().owner_id.as_deref()?),
            parent_digest: replay.parent_digest()?.to_owned(),
            child_digest: replay.decision_digest().to_owned(),
            outcome: outcome.clone(),
        })
    }
}

impl DecisionStoreWriter {
    pub(crate) fn record_child_outcome(&self, record: ChildOutcomeLink) -> Enqueue {
        let lineage = CausalLineage {
            workspace_id: Arc::clone(&self.workspace_id),
            owner_id: Some(Arc::clone(&record.owner)),
            decision_id: Some(Arc::from(record.parent.to_string())),
            ..CausalLineage::default()
        };
        if !record.outcome.valid_for_queue() || record.owner.is_empty() {
            emit_incomplete(self.events.as_ref(), lineage, IncompleteReason::Conflict);
            return Enqueue::Dropped;
        }
        match self.queue.try_send(WriterMessage::ChildOutcome(Box::new(record))) {
            Ok(()) => Enqueue::Queued,
            Err(_) => {
                emit_incomplete(self.events.as_ref(), lineage, IncompleteReason::StoreDropped);
                Enqueue::Dropped
            }
        }
    }
}

fn facts_match(conn: &Connection, scope: &DecisionScope, record: &ChildOutcomeLink) -> Result<bool, StoreError> {
    let Some(parent) = replay_record_on_connection(conn, scope, &record.parent)? else {
        return Ok(false);
    };
    let Some(child) = replay_record_on_connection(conn, scope, &record.child)? else {
        return Ok(false);
    };
    Ok(parent.digest == record.parent_digest
        && child.digest == record.child_digest
        && parent.decision.admits_issued_child()
        && parent.decision.enabled_snapshot
        && child.decision.enabled_snapshot
        && !parent.decision.legacy_v1
        && !child.decision.legacy_v1
        && outcome_matches_on_connection(conn, scope, &record.outcome)?)
}

fn stored_link_matches(conn: &Connection, record: &ChildOutcomeLink) -> Result<bool, StoreError> {
    let hash = EncodedOutcome::of(&record.outcome)?.record_hash;
    Ok(conn.query_row(
        "SELECT parent_decision_id, parent_digest, child_digest, outcome_hash FROM child_outcomes WHERE child_decision_id = ?1 AND settlement_id = ?2",
        params![record.child.to_string(), record.outcome.settlement_id],
        |row| Ok(row.get::<_, String>(0)? == record.parent.to_string()
            && row.get::<_, String>(1)? == record.parent_digest
            && row.get::<_, String>(2)? == record.child_digest
            && row.get::<_, String>(3)? == hash),
    ).optional()?.unwrap_or(false))
}

impl DecisionStoreReader {
    pub(crate) fn child_outcome_matches(
        &self,
        scope: &DecisionScope,
        record: &ChildOutcomeLink,
    ) -> Result<bool, StoreError> {
        self.with(false, |conn| {
            if store_schema::version(conn)? < 5 {
                return Ok(false);
            }
            let transaction = conn.unchecked_transaction()?;
            let matches = facts_match(&transaction, scope, record)? && stored_link_matches(&transaction, record)?;
            transaction.commit()?;
            Ok(matches)
        })
    }
}

impl WriterThread {
    pub(super) fn write_child_outcome(&mut self, record: &ChildOutcomeLink) {
        let lineage = outcome_lineage(&self.conn, record.parent).unwrap_or_else(|_| CausalLineage {
            decision_id: Some(Arc::from(record.parent.to_string())),
            ..CausalLineage::default()
        });
        let result = (|| -> Result<bool, StoreError> {
            let transaction: rusqlite::Transaction<'_> = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let workspace: String = transaction.query_row(
                "SELECT workspace_id FROM decisions WHERE decision_id = ?1",
                [record.parent.to_string()],
                |row| row.get(0),
            )?;
            let scope = DecisionScope::Owner {
                workspace_id: Arc::from(workspace),
                owner_id: Arc::clone(&record.owner),
            };
            if !facts_match(&transaction, &scope, record)? {
                return Ok(false);
            }
            let hash = EncodedOutcome::of(&record.outcome)?.record_hash;
            transaction.execute(
                "INSERT OR IGNORE INTO child_outcomes(parent_decision_id, child_decision_id, settlement_id, parent_digest, child_digest, outcome_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![record.parent.to_string(), record.child.to_string(), record.outcome.settlement_id, record.parent_digest, record.child_digest, hash],
            )?;
            let matches = stored_link_matches(&transaction, record)?;
            transaction.commit()?;
            Ok(matches)
        })();
        match result {
            Ok(true) => {}
            Ok(false) => emit_incomplete(self.events.as_ref(), lineage, IncompleteReason::Conflict),
            Err(error) => {
                tracing::warn!(error = %error, "child outcome link could not be persisted");
                emit_incomplete(self.events.as_ref(), lineage, IncompleteReason::StoreFailed);
            }
        }
    }
}

impl DecisionStoreReader {
    /// Exact parent/child/outcome/link readback under one bounded snapshot.
    /// Old or future schemas are errors, never an apparently absent link.
    pub(crate) fn child_outcome_matches_until(
        &self,
        scope: &DecisionScope,
        record: &ChildOutcomeLink,
        deadline: Instant,
    ) -> Result<bool, StoreError> {
        recovery_scope_bounds(scope)?;
        if record.owner.len() > 4096 || record.parent_digest.len() != 64 || record.child_digest.len() != 64 {
            return Err(StoreError::RecoveryBounds);
        }
        self.with_until(deadline, |conn| {
            recovery_schema(conn, 5)?;
            let transaction = conn.unchecked_transaction()?;
            let Some(parent) = replay_record_on_connection_until(&transaction, scope, &record.parent, deadline)? else {
                return Ok(false);
            };
            let Some(child) = replay_record_on_connection_until(&transaction, scope, &record.child, deadline)? else {
                return Ok(false);
            };
            if parent.digest != record.parent_digest
                || child.digest != record.child_digest
                || !parent.decision.admits_issued_child()
                || !parent.decision.enabled_snapshot
                || !child.decision.enabled_snapshot
                || parent.decision.legacy_v1
                || child.decision.legacy_v1
                || !outcome_matches_on_connection_until(&transaction, scope, &record.outcome, deadline)?
            {
                return Ok(false);
            }
            let size = recovery_size_sql(
                ["parent_decision_id", "parent_digest", "child_digest", "outcome_hash"].into_iter(),
                "c",
            );
            let bytes: Option<i64> = transaction
                .query_row(
                    &format!("SELECT {size} FROM child_outcomes c WHERE child_decision_id = ?1 AND settlement_id = ?2"),
                    params![record.child.to_string(), record.outcome.settlement_id],
                    |row| row.get(0),
                )
                .optional()?;
            match bytes {
                None => return Ok(false),
                Some(bytes) if !(0..=4096).contains(&bytes) => return Err(StoreError::RecoveryBounds),
                Some(_) => {}
            }
            recovery_time_left(deadline)?;
            let matches = stored_link_matches(&transaction, record)?;
            recovery_time_left(deadline)?;
            transaction.commit()?;
            Ok(matches)
        })
    }
}
