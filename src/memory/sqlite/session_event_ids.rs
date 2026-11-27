//! SQL of `Memory::message_event_ids_in_session`: which of a bounded list of
//! event ids are `message_events` rows of the principal's own session. Reads
//! ids only, never content, through the `event_id` unique index and the
//! `(workspace_id, session_key, id)` session index.

use super::SqliteMemory;
use crate::memory::traits::MemoryPrincipal;
use std::collections::HashSet;

/// The most ids one query binds; recall passes at most its candidate count.
const MAX_IDS: usize = 64;

impl SqliteMemory {
    pub(super) async fn message_event_ids_in_session_impl(
        &self,
        principal: &MemoryPrincipal,
        event_ids: &[String],
    ) -> anyhow::Result<HashSet<String>> {
        let sessions = principal.session_key_candidates();
        if event_ids.is_empty() || sessions.is_empty() {
            return Ok(HashSet::new());
        }
        let workspace = principal.workspace_id.clone();
        let ids = event_ids.iter().take(MAX_IDS).cloned().collect::<Vec<_>>();
        let pool = self.pool.clone();
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<HashSet<String>> {
            let reader = pool.read()?;
            let conn = reader.connection()?;
            let session_markers = (0..sessions.len())
                .map(|at| format!("?{}", at + 2))
                .collect::<Vec<_>>()
                .join(", ");
            let id_markers = (0..ids.len())
                .map(|at| format!("?{}", at + 2 + sessions.len()))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT event_id FROM message_events
                 WHERE workspace_id = ?1 AND session_key IN ({session_markers})
                   AND event_id IN ({id_markers})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let params = std::iter::once(&workspace).chain(sessions.iter()).chain(ids.iter());
            let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| row.get::<_, String>(0))?;
            let mut found = HashSet::new();
            for row in rows {
                found.insert(row?);
            }
            Ok(found)
        })
        .await?
    }
}
