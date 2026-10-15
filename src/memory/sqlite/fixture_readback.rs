pub(crate) struct TerminalRow {
    #[cfg(feature = "llm-router")]
    pub(crate) run_id: String,
    pub(crate) owner_id: Option<String>,
    pub(crate) raw_payload: String,
}
pub(crate) fn terminal_rows(workspace: &std::path::Path) -> rusqlite::Result<Vec<TerminalRow>> {
    let connection = rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    connection.prepare("SELECT run_id, owner_id, raw_payload_json FROM message_events WHERE event_type='turn.finalized' ORDER BY id")?
        .query_map([], |row| Ok(TerminalRow { #[cfg(feature = "llm-router")] run_id: row.get(0)?, owner_id: row.get(1)?, raw_payload: row.get(2)? }))?
        .collect()
}
pub(crate) fn tool_event_count(workspace: &std::path::Path) -> rusqlite::Result<i64> {
    let connection = rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    connection.query_row("SELECT count(*) FROM message_events WHERE event_type IN ('tool.execution.reserved','tool.execution.finalized')", [], |row| row.get(0))
}
#[cfg(feature = "llm-router")]
pub(crate) fn router_consumption_count(
    workspace: &std::path::Path,
    run: &str,
    source_workspace: &str,
    owner: &str,
    source: &str,
) -> rusqlite::Result<i64> {
    let connection = rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    connection.query_row(
        "SELECT COUNT(*) FROM router_learning_receipts r
             JOIN router_learning_state l ON l.domain=r.domain
             JOIN message_events t ON t.run_id=r.terminal AND t.workspace_id=r.workspace AND t.owner_id=r.owner
             JOIN message_events s ON s.event_id=t.causation_event_id
             WHERE r.terminal=?1 AND r.workspace=?2 AND r.owner=?3
               AND length(r.domain)=64 AND length(r.fingerprint)=64 AND l.version=1
               AND t.event_type='turn.finalized' AND s.event_id=?4
               AND s.event_type='message.created' AND s.role='user'
               AND s.owner_id=t.owner_id AND s.workspace_id=t.workspace_id
               AND s.session_key=t.session_key AND s.run_id=t.run_id AND s.id<t.id",
        rusqlite::params![run, source_workspace, owner, source],
        |row| row.get(0),
    )
}

/// Fixed observations of the actual Automix receipts; connection stays private.
#[cfg(feature = "llm-router")]
pub(crate) struct AutomixReadback {
    connection: rusqlite::Connection,
}
#[cfg(feature = "llm-router")]
impl AutomixReadback {
    pub(crate) fn open(workspace: &std::path::Path) -> rusqlite::Result<Self> {
        Ok(Self {
            connection: rusqlite::Connection::open_with_flags(
                workspace.join("memory/brain.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?,
        })
    }
    pub(crate) fn auxiliary_count(&self) -> rusqlite::Result<i64> {
        self.connection.query_row(
            "SELECT COUNT(*) FROM message_events WHERE event_type='provider.auxiliary.settled'",
            [],
            |row| row.get(0),
        )
    }
    pub(crate) fn auxiliary_payload(&self) -> rusqlite::Result<String> {
        self.connection.query_row(
            "SELECT raw_payload_json FROM message_events WHERE event_type='provider.auxiliary.settled'",
            [],
            |row| row.get(0),
        )
    }
    pub(crate) fn auxiliary_terminal_count(&self, run: &str, workspace: &str) -> rusqlite::Result<i64> {
        self.connection.query_row(
            "SELECT COUNT(*) FROM message_events a JOIN message_events t
             ON a.workspace_id=t.workspace_id AND a.owner_id=t.owner_id
               AND a.session_key=t.session_key AND a.run_id=t.run_id
             WHERE a.event_type='provider.auxiliary.settled' AND t.event_type='turn.finalized'
               AND a.id<t.id AND t.run_id=?1 AND a.workspace_id=?2
               AND a.idempotency_key=('turn:' || t.run_id || ':automix_premium')",
            rusqlite::params![run, workspace],
            |row| row.get(0),
        )
    }
    pub(crate) fn learning_state(&self) -> rusqlite::Result<(String, String)> {
        self.connection
            .query_row("SELECT domain,snapshot FROM router_learning_state", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
    }
    pub(crate) fn learning_receipt_count(&self) -> rusqlite::Result<i64> {
        self.connection
            .query_row("SELECT COUNT(*) FROM router_learning_receipts", [], |row| row.get(0))
    }
    pub(crate) fn learning_terminal_count(&self, run: &str, domain: &str, workspace: &str) -> rusqlite::Result<i64> {
        self.connection.query_row(
            "SELECT COUNT(*) FROM router_learning_receipts r
             JOIN message_events t ON t.run_id=r.terminal AND t.workspace_id=r.workspace AND t.owner_id=r.owner
             JOIN message_events s ON s.event_id=t.causation_event_id
             WHERE r.terminal=?1 AND r.domain=?2 AND r.workspace=?3
               AND length(r.fingerprint)=64 AND t.event_type='turn.finalized'
               AND s.event_type='message.created' AND s.role='user'
               AND s.owner_id=t.owner_id AND s.workspace_id=t.workspace_id
               AND s.session_key=t.session_key AND s.run_id=t.run_id AND s.id<t.id",
            rusqlite::params![run, domain, workspace],
            |row| row.get(0),
        )
    }
}
