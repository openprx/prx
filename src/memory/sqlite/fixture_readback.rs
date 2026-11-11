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

/// Fixed observations of the actual Router receipts; connection stays private.
#[cfg(feature = "llm-router")]
pub(crate) struct RouterReadback {
    connection: rusqlite::Connection,
}
#[cfg(feature = "llm-router")]
impl RouterReadback {
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

fn read_only(workspace: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(
        workspace.join("memory/brain.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("test: brain.db")
}

pub(crate) fn table_exists(workspace: &std::path::Path, table: &str) -> bool {
    read_only(workspace)
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name=?1",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .expect("test: schema query")
        > 0
}

pub(crate) fn row_count(workspace: &std::path::Path, table: &str) -> i64 {
    read_only(workspace)
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
        .expect("test: row count")
}

#[derive(Debug, Clone)]
pub(crate) struct OutboxRow {
    pub(crate) settlement_id: String,
    pub(crate) owner_id: String,
    pub(crate) pricing: String,
    pub(crate) state: String,
    pub(crate) facts: Vec<u8>,
}

pub(crate) fn outbox_rows(workspace: &std::path::Path) -> Vec<OutboxRow> {
    if !table_exists(workspace, "recovery_cost_outbox") {
        return Vec::new();
    }
    read_only(workspace)
        .prepare(
            "SELECT settlement_id, owner_id, pricing, state, facts FROM recovery_cost_outbox ORDER BY settlement_id",
        )
        .expect("test: outbox statement")
        .query_map([], |row| {
            Ok(OutboxRow {
                settlement_id: row.get(0)?,
                owner_id: row.get(1)?,
                pricing: row.get(2)?,
                state: row.get(3)?,
                facts: row.get(4)?,
            })
        })
        .expect("test: outbox query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("test: outbox rows")
}

pub(crate) fn claim_payloads(workspace: &std::path::Path) -> Vec<String> {
    read_only(workspace)
        .prepare("SELECT raw_payload_json FROM message_events WHERE event_type='turn.decision.claimed' ORDER BY id")
        .expect("test: claim statement")
        .query_map([], |row| row.get::<_, Option<String>>(0))
        .expect("test: claim query")
        .map(|row| row.expect("test: claim row").unwrap_or_default())
        .collect()
}

pub(crate) fn stored_contents(workspace: &std::path::Path, role: &str) -> Vec<String> {
    read_only(workspace)
        .prepare("SELECT content FROM message_events WHERE role=?1 ORDER BY id")
        .expect("test: content statement")
        .query_map([role], |row| row.get::<_, String>(0))
        .expect("test: content query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("test: content rows")
}

pub(crate) fn indexed_tables(workspace: &std::path::Path) -> Vec<String> {
    if !table_exists(workspace, "ramflux_erase_index") {
        return Vec::new();
    }
    let mut tables = read_only(workspace)
        .prepare("SELECT DISTINCT table_name FROM ramflux_erase_index ORDER BY table_name")
        .expect("test: erase index")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("test: erase index query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("test: erase index rows");
    tables.sort();
    tables.dedup();
    tables
}

/// Observe the terminal-origin inheritance independently of the reception origin.
pub(crate) fn erasure_index_rows_for(
    workspace: &std::path::Path,
    table: &str,
    origin: &crate::memory::erasure::ErasureOrigin,
) -> i64 {
    read_only(workspace).query_row(
        "SELECT count(*) FROM ramflux_erase_index WHERE table_name=?1 AND account=?2 AND conversation=?3 AND message=?4",
        rusqlite::params![table, origin.account, origin.conversation, origin.message], |row| row.get(0)
    ).expect("test: origin index rows")
}
