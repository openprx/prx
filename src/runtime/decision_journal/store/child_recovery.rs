//! Bounded discovery of retained decisions, not proof of child completion.
use super::{
    DecisionId, DecisionScope, DecisionStoreReader, Instant, StoreError, params, recovery_schema,
    recovery_scope_bounds, recovery_time_left,
};

/// SQLite row identity anchors an indexed (time, decision-id) continuation.
/// Keeping only a number also permits advancing past oversized/corrupt IDs.
#[derive(Clone, Copy)]
pub(crate) struct ChildRecoveryCursor {
    after_row_id: i64,
    high_water_row_id: i64,
}

pub(crate) struct ChildRecoveryCandidate {
    pub(crate) cursor: ChildRecoveryCursor,
    pub(crate) lineage: Option<ChildRecoveryLineage>,
}

pub(crate) struct ChildRecoveryLineage {
    pub(crate) decision_id: DecisionId,
    pub(crate) source_event_id: String,
    pub(crate) run_id: String,
    pub(crate) session_key: String,
}

impl DecisionStoreReader {
    pub(crate) fn child_recovery_candidates_until(
        &self,
        scope: &DecisionScope,
        after: Option<ChildRecoveryCursor>,
        limit: usize,
        deadline: Instant,
    ) -> Result<Vec<ChildRecoveryCandidate>, StoreError> {
        recovery_scope_bounds(scope)?;
        let DecisionScope::Owner { workspace_id, owner_id } = scope else {
            return Err(StoreError::RecoveryBounds);
        };
        if owner_id.is_empty() || !(1..=64).contains(&limit) {
            return Err(StoreError::RecoveryBounds);
        }
        self.with_until(deadline, |conn| {
            recovery_schema(conn, 5)?;
            // Fence this pass at an insertion watermark so ongoing appends
            // cannot postpone wrap forever. Next pass includes late commits,
            // including those whose creation timestamp sorts behind us.
            let high_water = match after {
                Some(cursor) => cursor.high_water_row_id,
                None => conn.query_row("SELECT COALESCE(MAX(rowid), 0) FROM decisions", [], |row| row.get(0))?,
            };
            // Oversized fields never cross the SQL/Rust boundary. An invalid
            // candidate remains explicit, with a numeric continuation token.
            let bounds = "typeof(d.decision_id) = 'text' AND typeof(d.source_event_id) = 'text' AND typeof(d.run_id) = 'text' AND typeof(d.session_key) = 'text' AND length(CAST(d.decision_id AS BLOB)) BETWEEN 1 AND 4096 AND length(CAST(d.source_event_id AS BLOB)) BETWEEN 1 AND 4096 AND length(CAST(d.run_id AS BLOB)) BETWEEN 1 AND 4096 AND length(CAST(d.session_key AS BLOB)) BETWEEN 1 AND 4096";
            let continuation = if after.is_some() {
                "AND (d.created_at_ms, d.decision_id) > (SELECT c.created_at_ms, c.decision_id FROM decisions c WHERE c.rowid = ?3 AND c.workspace_id = ?1 AND c.owner_id = ?2)"
            } else { "AND ?3 IS NULL" };
            let sql = format!(
                "SELECT d.rowid, CASE WHEN {bounds} THEN d.decision_id END, CASE WHEN {bounds} THEN d.source_event_id END, CASE WHEN {bounds} THEN d.run_id END, CASE WHEN {bounds} THEN d.session_key END FROM decisions d WHERE d.workspace_id = ?1 AND d.owner_id = ?2 AND d.rowid <= ?5 {continuation} ORDER BY d.created_at_ms, d.decision_id LIMIT ?4"
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let mut rows = statement.query(params![workspace_id.as_ref(), owner_id.as_ref(), after.map(|cursor| cursor.after_row_id), limit as i64, high_water])?;
            let mut candidates = Vec::with_capacity(limit);
            while let Some(row) = rows.next()? {
                recovery_time_left(deadline)?;
                let cursor = ChildRecoveryCursor { after_row_id: row.get(0)?, high_water_row_id: high_water };
                let id = row.get_ref(1)?.as_str().ok().map(str::to_owned);
                let source = row.get_ref(2)?.as_str().ok().map(str::to_owned);
                let run = row.get_ref(3)?.as_str().ok().map(str::to_owned);
                let session = row.get_ref(4)?.as_str().ok().map(str::to_owned);
                let lineage = match (id, source, run, session) {
                    (Some(id), Some(source_event_id), Some(run_id), Some(session_key)) => {
                        DecisionId::parse(&id).ok().map(|decision_id| ChildRecoveryLineage {
                            decision_id, source_event_id, run_id, session_key,
                        })
                    }
                    _ => None,
                };
                candidates.push(ChildRecoveryCandidate { cursor, lineage });
            }
            recovery_time_left(deadline)?;
            // A retained cursor deleted by retention produces an empty page;
            // the caller wraps, never treating that as exhaustive acceptance.
            Ok(candidates)
        })
    }
}
