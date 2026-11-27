//! SQLite's durable ingress projection and locally fenced lifetime ownership.
//! All message/projection mutations share the existing append transaction.
use super::{MessageEvent, MessageEventInput, SqliteConnectionPool, SqliteMemory};
use crate::memory::ingress_recovery::{
    AtomicIngressMutation, IngressCapsule, IngressEventLookup, IngressLifetime, IngressRecoveryCandidate,
    IngressRecoveryScope, IngressRecoveryState, MAX_CAPSULE_BYTES, MAX_ID_BYTES, MAX_RECOVERY_EVENT_BYTES,
    MAX_RECOVERY_PAGE, OrphanCause, remaining,
};
use crate::runtime::sqlite_pool::begin_immediate;
use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

thread_local! {
    static IO_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandlerFailStage {
    BeforeBusy,
    AfterBusy,
    AfterProgress,
}

#[cfg(test)]
thread_local! {
    static FAIL_HANDLER_STAGE: Cell<Option<HandlerFailStage>> = const { Cell::new(None) };
    static DEADLINE_BUSY_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
fn handler_fail_due(stage: HandlerFailStage) -> bool {
    FAIL_HANDLER_STAGE.with(|cell| {
        if cell.get() == Some(stage) {
            cell.set(None);
            true
        } else {
            false
        }
    })
}

fn wait_for_lock(_attempt: i32) -> bool {
    let left = IO_DEADLINE
        .with(Cell::get)
        .and_then(|deadline| remaining(deadline).ok());
    left.is_some_and(|left| {
        std::thread::sleep(left.min(Duration::from_millis(5)));
        IO_DEADLINE
            .with(Cell::get)
            .is_some_and(|deadline| crate::runtime::logical_clock::deadline_now(deadline) < deadline)
    })
}

/// Restores the pool's normal handler before its writer is returned. Rollback
/// drops before this guard, so even failed transactions retain the deadline.
///
/// rusqlite 0.41 installs handlers through `&mut self`. Callers shadow the
/// connection with this guard and use it via Deref/DerefMut.
pub(super) struct ConnectionDeadline<'a> {
    connection: &'a mut Connection,
    previous: Option<Instant>,
}
impl<'a> ConnectionDeadline<'a> {
    pub(super) fn new(connection: &'a mut Connection, deadline: Instant) -> anyhow::Result<Self> {
        remaining(deadline)?;
        let previous = IO_DEADLINE.with(|cell| cell.replace(Some(deadline)));
        let guard = Self { connection, previous };
        #[cfg(test)]
        if handler_fail_due(HandlerFailStage::BeforeBusy) {
            anyhow::bail!("test: handler install failed");
        }
        guard.connection.busy_handler(Some(wait_for_lock))?;
        #[cfg(test)]
        DEADLINE_BUSY_INSTALLED.with(|cell| cell.set(true));
        #[cfg(test)]
        if handler_fail_due(HandlerFailStage::AfterBusy) {
            anyhow::bail!("test: handler install failed");
        }
        guard.connection.progress_handler(
            1_000,
            Some(move || crate::runtime::logical_clock::deadline_now(deadline) >= deadline),
        )?;
        #[cfg(test)]
        if handler_fail_due(HandlerFailStage::AfterProgress) {
            anyhow::bail!("test: handler install failed");
        }
        Ok(guard)
    }
}
impl std::ops::Deref for ConnectionDeadline<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.connection
    }
}
impl std::ops::DerefMut for ConnectionDeadline<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.connection
    }
}
impl Drop for ConnectionDeadline<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.connection.progress_handler(0, None::<fn() -> bool>) {
            tracing::error!(error = %crate::agent::diagnostics::error_class(&error), "SQLite ingress progress handler reset failed");
        }
        match crate::runtime::sqlite_pool::install_busy_handler(self.connection) {
            Ok(()) => {
                #[cfg(test)]
                DEADLINE_BUSY_INSTALLED.with(|cell| cell.set(false));
            }
            Err(error) => {
                tracing::error!(error = %crate::agent::diagnostics::error_class(&error), "SQLite ingress busy handler reset failed");
            }
        }
        IO_DEADLINE.with(|cell| cell.set(self.previous));
    }
}

/// Optional deadline wrapper so a caller can keep one `&Connection` whether a
/// [`ConnectionDeadline`] is installed or not. Exclusive match arms avoid
/// overlapping the guarded `&mut` with a plain borrow of the same connection.
pub(super) enum DeadlineConn<'a> {
    Guarded(ConnectionDeadline<'a>),
    Plain(&'a Connection),
}
impl DeadlineConn<'_> {
    pub(super) fn wrap(connection: &mut Connection, deadline: Option<Instant>) -> anyhow::Result<DeadlineConn<'_>> {
        if let Some(deadline) = deadline {
            Ok(DeadlineConn::Guarded(ConnectionDeadline::new(connection, deadline)?))
        } else {
            Ok(DeadlineConn::Plain(connection))
        }
    }
}
impl std::ops::Deref for DeadlineConn<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            Self::Guarded(guard) => guard,
            Self::Plain(connection) => connection,
        }
    }
}

pub(super) const fn deadline(operation: &AtomicIngressMutation) -> Instant {
    operation.deadline()
}

fn scope_bounds(scope: &IngressRecoveryScope) -> anyhow::Result<()> {
    for value in [&scope.workspace_id, &scope.owner_id] {
        anyhow::ensure!(
            !value.is_empty() && value.len() <= MAX_ID_BYTES,
            "invalid ingress scope size"
        );
    }
    anyhow::ensure!(
        !matches!(scope.source, crate::memory::MessageEventSource::Other(_)),
        "unsupported ingress recovery source"
    );
    Ok(())
}

/// Lazy migration: ordinary/off message writers never call this function.
/// It reads and then may write (the first use creates the tables; every use
/// may add the scope column and the erasure triggers), so callers run it in a
/// [`begin_immediate`] transaction.
pub(super) fn schema(connection: &Connection) -> anyhow::Result<String> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'ingress_recovery_schema')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        connection.execute_batch(
            "CREATE TABLE ingress_recovery_schema (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1), version INTEGER NOT NULL,
                 backend_namespace TEXT NOT NULL
             );
             CREATE TABLE ingress_recovery (
                 cursor INTEGER PRIMARY KEY REFERENCES message_events(id),
                 input_event_id TEXT NOT NULL UNIQUE REFERENCES message_events(event_id),
                 workspace_id TEXT NOT NULL, owner_id TEXT NOT NULL, source TEXT NOT NULL,
                 input_key TEXT NOT NULL, lifetime TEXT NOT NULL,
                 capsule_json TEXT NOT NULL CHECK(length(CAST(capsule_json AS BLOB)) <= 16384),
                 state TEXT NOT NULL CHECK(state IN ('pending', 'unconfirmed', 'reconciled')),
                 evidence_event_id TEXT REFERENCES message_events(event_id),
                 UNIQUE(workspace_id, input_key)
             );
             CREATE INDEX ingress_recovery_scope_cursor
                 ON ingress_recovery(workspace_id, owner_id, source, state, lifetime, cursor)
                 WHERE state <> 'reconciled';",
        )?;
        connection.execute(
            "INSERT INTO ingress_recovery_schema(singleton, version, backend_namespace) VALUES(1, 1, ?1)",
            [uuid::Uuid::new_v4().to_string()],
        )?;
    }
    let (version, namespace): (i64, String) = connection.query_row(
        "SELECT version, CASE WHEN length(backend_namespace) = 36 THEN backend_namespace ELSE NULL END
         FROM ingress_recovery_schema WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    anyhow::ensure!(version == 1, "unsupported SQLite ingress recovery schema");
    super::agent_registry::add_scope_column(connection, "ingress_recovery")?;
    anyhow::ensure!(
        uuid::Uuid::parse_str(&namespace)?.to_string() == namespace,
        "invalid SQLite ingress backend namespace"
    );
    super::erasure_pool::install_table_triggers(connection, "ingress_recovery")?;
    Ok(namespace)
}

fn encode_capsule(capsule: &IngressCapsule) -> anyhow::Result<String> {
    capsule.validate_bounds()?;
    anyhow::ensure!(
        capsule.input_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            && capsule.authentication.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid ingress authentication encoding"
    );
    let encoded = serde_json::to_string(capsule)?;
    anyhow::ensure!(encoded.len() <= MAX_CAPSULE_BYTES, "ingress capsule exceeds byte limit");
    Ok(encoded)
}

fn event_scope(event: &MessageEvent, scope: &IngressRecoveryScope) -> anyhow::Result<()> {
    anyhow::ensure!(
        event.workspace_id == scope.workspace_id
            && event.owner_id.as_deref() == Some(scope.owner_id.as_str())
            && event.source == scope.source,
        "ingress event scope mismatch"
    );
    Ok(())
}

struct EncodedByteLimit {
    remaining: usize,
}

impl std::io::Write for EncodedByteLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self.remaining.checked_sub(bytes.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ingress event exceeds recovery byte limit",
            )
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reject oversized owned fields before JSON visits them or a caller clones
/// the proposal. The writer then counts escaping/JSON overhead without ever
/// allocating an encoded event buffer.
pub(super) fn validate_input_bounds(input: &MessageEventInput) -> anyhow::Result<()> {
    let mut raw_bytes = 0usize;
    let mut add = |value: &str, limit: usize| -> anyhow::Result<()> {
        anyhow::ensure!(value.len() <= limit, "ingress field exceeds recovery byte limit");
        raw_bytes = raw_bytes
            .checked_add(value.len())
            .filter(|total| *total <= MAX_RECOVERY_EVENT_BYTES)
            .context("ingress event exceeds recovery byte limit")?;
        Ok(())
    };
    for value in [
        input.workspace_id.as_str(),
        input.source.as_str(),
        input.role.as_str(),
        input.event_type.as_str(),
        input.visibility.as_str(),
    ] {
        add(value, MAX_ID_BYTES)?;
    }
    for value in [
        &input.event_id,
        &input.idempotency_key,
        &input.owner_id,
        &input.channel,
        &input.session_key,
        &input.parent_session_key,
        &input.run_id,
        &input.parent_run_id,
        &input.agent_id,
        &input.persona_id,
        &input.sender,
        &input.recipient,
        &input.goal_id,
        &input.causation_event_id,
        &input.correlation_id,
        &input.attempt_id,
        &input.config_source_revision,
    ]
    .into_iter()
    .flatten()
    {
        add(value, MAX_ID_BYTES)?;
    }
    use crate::memory::traits::MessageEventSubject;
    if let Some(subject) = &input.subject {
        match subject {
            MessageEventSubject::Project(value)
            | MessageEventSubject::Task(value)
            | MessageEventSubject::Incident(value)
            | MessageEventSubject::Customer(value)
            | MessageEventSubject::Document(value)
            | MessageEventSubject::Goal(value)
            | MessageEventSubject::Topic(value)
            | MessageEventSubject::Conversation(value) => add(value, MAX_ID_BYTES)?,
            MessageEventSubject::Other { subject_type, id } => {
                add(subject_type, MAX_ID_BYTES)?;
                add(id, MAX_ID_BYTES)?;
            }
        }
    }
    add(&input.content, MAX_RECOVERY_EVENT_BYTES)?;
    if let Some(payload) = &input.raw_payload_json {
        add(payload, MAX_RECOVERY_EVENT_BYTES)?;
    }
    serde_json::to_writer(
        EncodedByteLimit {
            remaining: MAX_RECOVERY_EVENT_BYTES,
        },
        input,
    )?;
    Ok(())
}

fn input_scope(input: &MessageEventInput, scope: &IngressRecoveryScope) -> anyhow::Result<()> {
    scope_bounds(scope)?;
    anyhow::ensure!(
        input.workspace_id == scope.workspace_id
            && input.owner_id.as_deref() == Some(scope.owner_id.as_str())
            && input.source == scope.source,
        "ingress input scope mismatch"
    );
    validate_input_bounds(input)?;
    Ok(())
}

fn stored_capsule(connection: &Connection, event_id: &str) -> anyhow::Result<IngressCapsule> {
    let size: i64 = connection.query_row(
        "SELECT length(CAST(capsule_json AS BLOB)) FROM ingress_recovery WHERE input_event_id = ?1",
        [event_id],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        (1..=MAX_CAPSULE_BYTES as i64).contains(&size),
        "invalid persisted ingress capsule size"
    );
    let encoded: String = connection.query_row(
        "SELECT capsule_json FROM ingress_recovery WHERE input_event_id = ?1",
        [event_id],
        |row| row.get(0),
    )?;
    IngressCapsule::decode(&encoded)
}

pub(in crate::memory::sqlite) fn before_append(
    transaction: &Transaction<'_>,
    operation: &AtomicIngressMutation,
    input: &MessageEventInput,
) -> anyhow::Result<()> {
    remaining(deadline(operation))?;
    let capsule = operation.capsule();
    encode_capsule(capsule)?;
    input_scope(input, &capsule.scope)?;
    validate_lease(transaction, operation)?;
    let namespace = schema(transaction)?;
    anyhow::ensure!(
        namespace == capsule.backend_namespace,
        "ingress capsule backend namespace mismatch"
    );
    match operation {
        AtomicIngressMutation::Input { .. } => {
            anyhow::ensure!(
                input.event_id.as_deref() == Some(capsule.input_event_id.as_str())
                    && input.idempotency_key.as_deref() == Some(capsule.input_key.as_str())
                    && input.run_id.as_deref() == Some(capsule.run_id.as_str())
                    && input.event_type == "message.created"
                    && input.role == "user",
                "ingress capsule differs from input identity"
            );
            let wrapper: serde_json::Value = serde_json::from_str(
                input
                    .raw_payload_json
                    .as_deref()
                    .context("ingress capsule wrapper missing")?,
            )?;
            anyhow::ensure!(
                wrapper.get("prx_ingress_capsule") == Some(&serde_json::to_value(capsule)?)
                    && wrapper
                        .get("original_payload_json")
                        .is_some_and(|value| value.is_null() || value.is_string()),
                "ingress capsule wrapper mismatch"
            );
        }
        AtomicIngressMutation::Recovery(commit)
        | AtomicIngressMutation::Owner(commit)
        | AtomicIngressMutation::Orphan(commit) => {
            anyhow::ensure!(
                encode_capsule(&stored_capsule(transaction, &capsule.input_event_id)?)? == encode_capsule(capsule)?,
                "recovery capsule differs from durable input"
            );
            // The owner only ever reconciles its own finished row.
            anyhow::ensure!(
                !matches!(operation, AtomicIngressMutation::Owner(_))
                    || (commit.state == IngressRecoveryState::Reconciled && commit.cause.is_none()),
                "owner settlement must reconcile its own row"
            );
            let state = state_label(commit.state)?;
            let expected_key = format!("ingress:{}:{state}", capsule.input_event_id);
            let event_type = match commit.state {
                IngressRecoveryState::Unconfirmed => "turn.ingress.interrupted",
                IngressRecoveryState::Reconciled => "turn.ingress.reconciled",
                IngressRecoveryState::Pending => anyhow::bail!("recovery cannot restore pending authority"),
            };
            anyhow::ensure!(
                input
                    .event_id
                    .as_ref()
                    .is_none_or(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
                    && input.idempotency_key.as_deref() == Some(expected_key.as_str())
                    && input.run_id.as_deref() == Some(capsule.run_id.as_str())
                    && input.causation_event_id.as_deref() == Some(capsule.input_event_id.as_str())
                    && input.role == "event"
                    && input.event_type == event_type,
                "recovery evidence identity mismatch"
            );
            let payload: serde_json::Value = serde_json::from_str(
                input
                    .raw_payload_json
                    .as_deref()
                    .context("recovery evidence payload missing")?,
            )?;
            let evidence = payload
                .get("ingress_recovery")
                .context("recovery evidence envelope missing")?;
            anyhow::ensure!(
                evidence.get("capsule") == Some(&serde_json::to_value(capsule)?)
                    && evidence.get("state").and_then(serde_json::Value::as_str) == Some(state)
                    && evidence
                        .get("evidence_authentication")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|mac| mac.len() == 64 && mac.bytes().all(|byte| byte.is_ascii_hexdigit()))
                    && crate::memory::ingress_recovery::evidence_cause_matches(evidence, commit.cause),
                "recovery evidence binding mismatch"
            );
            if commit.cause == Some(OrphanCause::InputMissing) {
                // The input event is gone or no longer carries a capsule. An
                // unparseable payload might still carry one: it is not orphaned.
                let input_present: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM message_events WHERE event_id = ?1
                        AND raw_payload_json IS NOT NULL AND (json_valid(raw_payload_json) = 0
                            OR json_type(raw_payload_json, '$.prx_ingress_capsule') IS NOT NULL))",
                    [&capsule.input_event_id],
                    |row| row.get(0),
                )?;
                anyhow::ensure!(!input_present, "orphaned ingress input still carries its capsule");
            } else {
                let input_exists: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM message_events WHERE event_id = ?1 AND workspace_id = ?2
                    AND owner_id = ?3 AND source = ?4 AND idempotency_key = ?5 AND run_id = ?6
                    AND event_type = 'message.created' AND role = 'user')",
                    params![
                        capsule.input_event_id,
                        capsule.scope.workspace_id,
                        capsule.scope.owner_id,
                        capsule.scope.source.as_str(),
                        capsule.input_key,
                        capsule.run_id
                    ],
                    |row| row.get(0),
                )?;
                anyhow::ensure!(input_exists, "recovery original input identity changed");
            }
            let previous: String = transaction.query_row(
                "SELECT state FROM ingress_recovery WHERE input_event_id = ?1 AND lifetime = ?2
                    AND workspace_id = ?3 AND owner_id = ?4 AND source = ?5",
                params![
                    capsule.input_event_id,
                    capsule.lifetime,
                    capsule.scope.workspace_id,
                    capsule.scope.owner_id,
                    capsule.scope.source.as_str()
                ],
                |row| row.get(0),
            )?;
            anyhow::ensure!(
                previous == state || previous == "pending" || (previous == "unconfirmed" && state == "reconciled"),
                "invalid ingress recovery transition"
            );
        }
    }
    remaining(deadline(operation))?;
    Ok(())
}

fn state_label(state: IngressRecoveryState) -> anyhow::Result<&'static str> {
    match state {
        IngressRecoveryState::Pending => anyhow::bail!("recovery cannot restore pending authority"),
        IngressRecoveryState::Unconfirmed => Ok("unconfirmed"),
        IngressRecoveryState::Reconciled => Ok("reconciled"),
    }
}

pub(in crate::memory::sqlite) fn after_append(
    transaction: &Transaction<'_>,
    operation: &AtomicIngressMutation,
    event: &MessageEvent,
    inserted: bool,
) -> anyhow::Result<()> {
    let capsule = operation.capsule();
    event_scope(event, &capsule.scope)?;
    match operation {
        AtomicIngressMutation::Input { .. } => {
            anyhow::ensure!(
                event.idempotency_key.as_deref() == Some(capsule.input_key.as_str())
                    && event.event_type == "message.created"
                    && event.role == "user",
                "ingress key collides with another event"
            );
            // A previous key is an immutable receipt, even if it predates the
            // capsule schema. Never retrofit the new lifetime into that row.
            if inserted {
                anyhow::ensure!(
                    event.event_id == capsule.input_event_id
                        && event.run_id.as_deref() == Some(capsule.run_id.as_str()),
                    "input receipt identity mismatch"
                );
                transaction.execute(
                    "INSERT INTO ingress_recovery(cursor, input_event_id, workspace_id, owner_id,
                        source, input_key, lifetime, capsule_json, state, ma_scope_id)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', (SELECT ma_scope_id FROM message_events WHERE event_id=?2))",
                    params![
                        event.id,
                        event.event_id,
                        capsule.scope.workspace_id,
                        capsule.scope.owner_id,
                        capsule.scope.source.as_str(),
                        capsule.input_key,
                        capsule.lifetime,
                        encode_capsule(capsule)?
                    ],
                )?;
            }
        }
        AtomicIngressMutation::Recovery(commit)
        | AtomicIngressMutation::Owner(commit)
        | AtomicIngressMutation::Orphan(commit) => {
            // Readback must match exactly; INSERT OR IGNORE is never authority
            // to update the projection after an unrelated id/key collision.
            let mut expected = serde_json::to_value(&commit.evidence)?;
            let mut actual = serde_json::to_value(event)?;
            for key in ["id", "content_hash", "created_at", "updated_at"] {
                actual
                    .as_object_mut()
                    .context("invalid event serialization")?
                    .remove(key);
            }
            // A replay may allocate a new evidence UUID; the stable key and
            // signed payload, rather than that new UUID, identify the receipt.
            if !inserted || commit.evidence.event_id.is_none() {
                *expected.get_mut("event_id").context("serialized event id missing")? =
                    serde_json::Value::String(event.event_id.clone());
            }
            anyhow::ensure!(actual == expected, "recovery evidence receipt mismatch");
            let state = state_label(commit.state)?;
            let changed = transaction.execute(
                "UPDATE ingress_recovery SET state = ?1, evidence_event_id = ?2
                 WHERE input_event_id = ?3 AND lifetime = ?4 AND workspace_id = ?5 AND owner_id = ?6
                   AND source = ?7 AND (state = ?1 OR state = 'pending'
                        OR (state = 'unconfirmed' AND ?1 = 'reconciled'))",
                params![
                    state,
                    event.event_id,
                    capsule.input_event_id,
                    capsule.lifetime,
                    capsule.scope.workspace_id,
                    capsule.scope.owner_id,
                    capsule.scope.source.as_str()
                ],
            )?;
            anyhow::ensure!(changed == 1, "recovery projection transition lost its fence");
        }
    }
    // Check the actual lease identity again immediately before COMMIT.
    validate_lease(transaction, operation)?;
    remaining(deadline(operation))?;
    Ok(())
}

pub(super) async fn list(
    pool: Arc<SqliteConnectionPool>,
    scope: IngressRecoveryScope,
    current: Arc<dyn IngressLifetime>,
    phase: crate::memory::ingress_recovery::IngressRecoveryScan,
    after_id: i64,
    limit: usize,
    deadline: Instant,
) -> anyhow::Result<Vec<IngressRecoveryCandidate>> {
    scope_bounds(&scope)?;
    anyhow::ensure!(
        after_id >= 0 && (1..=MAX_RECOVERY_PAGE).contains(&limit),
        "invalid recovery page bound"
    );
    remaining(deadline)?;
    crate::runtime::blocking::spawn_blocking(move || {
        let mut connection = pool.write_until(deadline)?;
        let connection = ConnectionDeadline::new(&mut connection, deadline)?;
        let transaction = begin_immediate(&connection)?;
        let namespace = schema(&transaction)?;
        validate_owner(&transaction,&current,&scope.workspace_id,deadline)?;
        let other_state=match phase {crate::memory::ingress_recovery::IngressRecoveryScan::PendingOnly=>"pending",crate::memory::ingress_recovery::IngressRecoveryScan::IncludeUnconfirmed=>"unconfirmed"};
        let candidates = {
            let projection="cursor, CASE WHEN length(CAST(capsule_json AS BLOB)) <= ?6 THEN capsule_json ELSE NULL END,
                CASE WHEN length(CAST(input_event_id AS BLOB))<=4096 THEN input_event_id ELSE NULL END,
                CASE WHEN length(CAST(input_key AS BLOB))<=4096 THEN input_key ELSE NULL END,
                CASE WHEN length(CAST(lifetime AS BLOB))<=36 THEN lifetime ELSE NULL END";
            // Two explicit indexed lifetime ranges exclude the entire current
            // owner's history before materialization or cursor ordering. Only
            // v3 capsules are candidates: earlier rows are skipped in SQL,
            // with no key read and no state change.
            let sql=format!("SELECT * FROM (
                SELECT {projection} FROM ingress_recovery INDEXED BY ingress_recovery_scope_cursor WHERE workspace_id=?1 AND owner_id=?2 AND source=?3 AND state IN ('pending',?8) AND state<>'reconciled' AND lifetime<?7 AND cursor>?4 AND json_extract(capsule_json,'$.version')=3
                UNION ALL
                SELECT {projection} FROM ingress_recovery INDEXED BY ingress_recovery_scope_cursor WHERE workspace_id=?1 AND owner_id=?2 AND source=?3 AND state IN ('pending',?8) AND state<>'reconciled' AND lifetime>?7 AND cursor>?4 AND json_extract(capsule_json,'$.version')=3
                ) ORDER BY cursor LIMIT ?5");
            let mut statement=transaction.prepare(&sql)?;
            let mut rows=statement.query(params![scope.workspace_id,scope.owner_id,scope.source.as_str(),after_id,i64::try_from(limit)?,MAX_CAPSULE_BYTES as i64,current.token(),other_state])?;
            let mut result = Vec::with_capacity(limit);
            while let Some(row) = rows.next()? {
                remaining(deadline)?;
                let encoded: Option<String> = row.get(1)?;
                let capsule = IngressCapsule::decode(encoded.as_deref().context("oversized ingress recovery capsule")?)?;
                let event_id: String = row.get(2)?;
                let input_key: String = row.get(3)?;
                let token: String = row.get(4)?;
                anyhow::ensure!(capsule.scope == scope && capsule.backend_namespace == namespace && capsule.input_event_id == event_id
                    && capsule.input_key == input_key && capsule.lifetime == token,
                    "recovery index differs from its capsule");
                result.push(IngressRecoveryCandidate { cursor: row.get(0)?, capsule });
            }
            result
        };
        remaining(deadline)?;
        transaction.commit()?;
        Ok(candidates)
    }).await?
}

const EVENT_COLUMNS: &str = "id, event_id, idempotency_key, workspace_id, owner_id, source, channel, session_key,
    parent_session_key, run_id, parent_run_id, agent_id, persona_id, sender, recipient, role, event_type,
    source_ref_json, subject_ref_json, goal_id, causation_event_id, correlation_id, attempt_id, lease_epoch,
    config_generation_id, config_source_revision, content, content_hash, raw_payload_json, visibility,
    created_at, updated_at";

fn event_size_expression() -> String {
    EVENT_COLUMNS
        .split(',')
        .map(|column| format!("COALESCE(length(CAST({} AS BLOB)), 0)", column.trim()))
        .collect::<Vec<_>>()
        .join(" + ")
}

pub(super) fn check_receipt_size(
    connection: &Connection,
    event_id: &str,
    key: Option<&str>,
    workspace: &str,
) -> anyhow::Result<()> {
    let size: i64 = connection.query_row(
        &format!(
            "SELECT MAX({}) FROM message_events WHERE event_id = ?1
            OR (workspace_id = ?3 AND idempotency_key = ?2)",
            event_size_expression()
        ),
        params![event_id, key, workspace],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        (0..=MAX_RECOVERY_EVENT_BYTES as i64).contains(&size),
        "ingress receipt exceeds recovery byte limit"
    );
    Ok(())
}

pub(super) async fn read_event(
    pool: Arc<SqliteConnectionPool>,
    scope: IngressRecoveryScope,
    lookup: IngressEventLookup,
    deadline: Instant,
) -> anyhow::Result<Option<MessageEvent>> {
    for value in [&scope.workspace_id, &scope.owner_id] {
        anyhow::ensure!(
            !value.is_empty() && value.len() <= MAX_ID_BYTES,
            "invalid ingress read scope"
        );
    }
    anyhow::ensure!(
        !scope.source.as_str().is_empty() && scope.source.as_str().len() <= MAX_ID_BYTES,
        "invalid ingress read source"
    );
    let (column, key, source_bound) = match lookup {
        IngressEventLookup::EventId(key) => ("event_id", key, true),
        IngressEventLookup::IdempotencyKey(key) => ("idempotency_key", key, true),
        IngressEventLookup::OwnedEventId(key) => ("event_id", key, false),
        IngressEventLookup::OwnedIdempotencyKey(key) => ("idempotency_key", key, false),
    };
    anyhow::ensure!(
        !key.is_empty() && key.len() <= MAX_ID_BYTES,
        "invalid ingress lookup size"
    );
    remaining(deadline)?;
    crate::runtime::blocking::spawn_blocking(move || {
        let mut connection = pool.write_until(deadline)?;
        let connection = ConnectionDeadline::new(&mut connection, deadline)?;
        let transaction = connection.unchecked_transaction()?;
        // SQLite measures every persisted column before Rust reads/decodes any
        // payload. The snapshot prevents a writer changing the row in between.
        let size_expression = event_size_expression();
        let source_filter = source_bound.then(|| scope.source.as_str());
        let predicate =
            format!("workspace_id = ?1 AND owner_id = ?2 AND (?3 IS NULL OR source = ?3) AND {column} = ?4");
        let size: Option<i64> = transaction
            .query_row(
                &format!("SELECT {size_expression} FROM message_events WHERE {predicate}"),
                params![scope.workspace_id, scope.owner_id, source_filter, key],
                |row| row.get(0),
            )
            .optional()?;
        let result = match size {
            None => None,
            Some(size) => {
                anyhow::ensure!(
                    (0..=MAX_RECOVERY_EVENT_BYTES as i64).contains(&size),
                    "persisted ingress event exceeds recovery byte limit"
                );
                let event = transaction.query_row(
                    &format!("SELECT {EVENT_COLUMNS} FROM message_events WHERE {predicate}"),
                    params![scope.workspace_id, scope.owner_id, source_filter, key],
                    SqliteMemory::message_event_from_row,
                )?;
                anyhow::ensure!(
                    event.workspace_id == scope.workspace_id
                        && event.owner_id.as_deref() == Some(scope.owner_id.as_str()),
                    "ingress read owner mismatch"
                );
                if source_bound {
                    event_scope(&event, &scope)?;
                }
                Some(event)
            }
        };
        remaining(deadline)?;
        transaction.commit()?;
        Ok(result)
    })
    .await?
}

// Unsupported systems fail only the new recovery capability. Existing SQLite
// message/history storage retains its platform contract.
#[cfg(not(unix))]
pub(super) async fn acquire(
    _pool: Arc<SqliteConnectionPool>,
    _database: PathBuf,
    _workspace: String,
    _token: String,
    _deadline: Instant,
    _recovery: bool,
) -> anyhow::Result<Option<Arc<dyn IngressLifetime>>> {
    anyhow::bail!("SQLite ingress recovery requires supported local Unix file locks")
}

#[cfg(not(unix))]
fn validate_lease(_connection: &Connection, _operation: &AtomicIngressMutation) -> anyhow::Result<()> {
    anyhow::bail!("SQLite ingress recovery requires supported local Unix file locks")
}

#[cfg(all(unix, test))]
pub(super) use local::{RECOVERY_DIRECTORY, lock_directory_name};
#[cfg(unix)]
pub(super) use local::{acquire, validate_lease, validate_owner};

#[cfg(unix)]
mod local {
    use super::{
        Arc, AtomicIngressMutation, Connection, ConnectionDeadline, Context, IngressLifetime, Instant, MAX_ID_BYTES,
        PathBuf, SqliteConnectionPool, SqliteMemory, begin_immediate, deadline, remaining, schema,
    };
    use crate::memory::ingress_recovery::IngressLockMissing;
    use fs2::FileExt;
    use parking_lot::Mutex;
    use rustix::fs::{AtFlags, Mode, OFlags};
    use std::collections::HashMap;
    use std::fs::{File, Metadata};
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::{OnceLock, Weak};

    struct Directory {
        path: PathBuf,
        file: File,
        private: bool,
    }

    struct Lease {
        token: String,
        namespace: String,
        workspace: String,
        database: PathBuf,
        database_file: File,
        directories: Vec<Directory>,
        lock_path: PathBuf,
        lock_file: File,
        uid: u32,
        recovery: bool,
    }

    /// The workspace directory that holds every SQLite ingress lock.
    pub(in super::super) const RECOVERY_DIRECTORY: &str = ".prx-ingress-recovery-v1";

    /// Name of one backend's lock directory. It depends only on stable
    /// identity: the canonical database path, its inode and the backend
    /// namespace. The device number is not stable (a reboot may renumber the
    /// volume), so it is not part of the name. Separate databases in one
    /// workspace still cannot arbitrate each other's runs.
    pub(in super::super) fn lock_directory_name(database: &Path, inode: u64, namespace: &str) -> String {
        SqliteMemory::content_hash(&format!("{}:{inode}:{namespace}", database.display()))
    }

    static ISSUED: OnceLock<Mutex<HashMap<usize, Weak<Lease>>>> = OnceLock::new();

    fn issued() -> &'static Mutex<HashMap<usize, Weak<Lease>>> {
        ISSUED.get_or_init(|| Mutex::new(HashMap::new()))
    }

    const MAX_LIVE_LEASES: usize = 128;

    fn registered(pointer: usize) -> anyhow::Result<Arc<Lease>> {
        let mut registry = issued().lock();
        registry.retain(|_, lease| lease.strong_count() != 0);
        registry
            .get(&pointer)
            .and_then(Weak::upgrade)
            .context("ingress lease was not issued by SQLite")
    }

    fn same_file(actual: &Metadata, expected: &Metadata) -> bool {
        actual.dev() == expected.dev() && actual.ino() == expected.ino()
    }

    fn check_metadata(metadata: &Metadata, uid: u32, directory: bool, private: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            metadata.uid() == uid
                && if directory {
                    metadata.is_dir()
                } else {
                    metadata.is_file() && metadata.nlink() == 1
                },
            "ingress lease object has unsafe type, ownership or links"
        );
        let disallowed = if private { 0o077 } else { 0o022 };
        anyhow::ensure!(
            metadata.mode() & disallowed == 0,
            "ingress lease object has unsafe permissions"
        );
        Ok(())
    }

    fn directory(path: &Path, private: bool, uid: u32) -> anyhow::Result<Directory> {
        let file = File::from(rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        check_metadata(&file.metadata()?, uid, true, private)?;
        anyhow::ensure!(
            same_file(&std::fs::symlink_metadata(path)?, &file.metadata()?),
            "ingress directory changed while opening"
        );
        Ok(Directory {
            path: path.to_owned(),
            file,
            private,
        })
    }

    fn child_directory(parent: &Directory, name: &str, create: bool, uid: u32) -> anyhow::Result<Directory> {
        if create {
            match rustix::fs::mkdirat(&parent.file, name, Mode::from_raw_mode(0o700)) {
                Ok(()) => parent.file.sync_all()?,
                Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let file = File::from(rustix::fs::openat(
            &parent.file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        check_metadata(&file.metadata()?, uid, true, true)?;
        let path = parent.path.join(name);
        anyhow::ensure!(
            same_file(&std::fs::symlink_metadata(&path)?, &file.metadata()?),
            "ingress directory changed while opening"
        );
        Ok(Directory {
            path,
            file,
            private: true,
        })
    }

    /// The kernel reported a filesystem magic number that is not a valid
    /// unsigned value, so it cannot match any admitted local filesystem.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[derive(Debug, thiserror::Error)]
    #[error("filesystem type reported by the kernel is out of range")]
    struct FilesystemTypeOutOfRange;

    /// Only known local filesystems are admitted. NFS/CIFS/FUSE and unknown
    /// types cannot supply this single-host lifetime proof.
    fn local_filesystem(file: &File) -> anyhow::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let stat = rustix::fs::fstatfs(file)?;
            let kind = u64::try_from(stat.f_type).map_err(|_| FilesystemTypeOutOfRange)?;
            anyhow::ensure!(
                matches!(
                    kind,
                    0xEF53 | 0x5846_5342 | 0x9123_683E | 0x0102_1994 | 0x794C_7630 | 0x2FC1_2FC1
                ),
                "filesystem does not provide supported local ingress locking"
            );
            Ok(())
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            let stat = rustix::fs::fstatfs(file)?;
            let name: Vec<u8> = stat
                .f_fstypename
                .iter()
                .take_while(|byte| **byte != 0)
                .map(|byte| *byte as u8)
                .collect();
            anyhow::ensure!(
                matches!(name.as_slice(), b"apfs" | b"hfs" | b"tmpfs"),
                "filesystem does not provide supported local ingress locking"
            );
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios")))]
        {
            let _ = file;
            anyhow::bail!("filesystem classification unavailable for SQLite ingress recovery")
        }
    }

    impl Lease {
        fn validate_sync(&self, deadline: Instant) -> anyhow::Result<()> {
            remaining(deadline)?;
            check_metadata(&self.database_file.metadata()?, self.uid, false, false)?;
            anyhow::ensure!(
                same_file(
                    &std::fs::symlink_metadata(&self.database)?,
                    &self.database_file.metadata()?
                ),
                "ingress database identity changed"
            );
            for directory in &self.directories {
                let opened = directory.file.metadata()?;
                check_metadata(&opened, self.uid, true, directory.private)?;
                let current = std::fs::symlink_metadata(&directory.path)?;
                anyhow::ensure!(
                    current.is_dir() && same_file(&current, &opened),
                    "ingress directory identity changed"
                );
            }
            let opened = self.lock_file.metadata()?;
            check_metadata(&opened, self.uid, false, true)?;
            let current = std::fs::symlink_metadata(&self.lock_path)?;
            anyhow::ensure!(
                current.is_file() && same_file(&current, &opened),
                "ingress lock identity changed"
            );
            remaining(deadline)?;
            Ok(())
        }

        /// Orphan authority: another lifetime's lock is absent from this
        /// lease's own (stable) lock directory. A lifetime creates its lock
        /// exactly once, before its first row, so an absent lock never returns.
        fn lock_absent(&self, token: &str) -> anyhow::Result<()> {
            anyhow::ensure!(
                uuid::Uuid::parse_str(token)?.to_string() == token && token != self.token,
                "orphaned ingress lifetime is not canonical"
            );
            let backend = self.directories.last().context("ingress lock directory missing")?;
            match rustix::fs::statat(
                &backend.file,
                format!("{token}.lock").as_str(),
                AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Err(rustix::io::Errno::NOENT) => Ok(()),
                Ok(_) => anyhow::bail!("orphaned ingress settlement found the lifetime lock present"),
                Err(error) => Err(error).context("orphaned ingress lock cannot be inspected"),
            }
        }
    }

    #[async_trait::async_trait]
    impl IngressLifetime for Lease {
        fn token(&self) -> &str {
            &self.token
        }
        fn namespace(&self) -> &str {
            &self.namespace
        }

        async fn validate(&self, deadline: Instant) -> anyhow::Result<()> {
            // Upgrade the issued object rather than retaining just a token. A
            // cancelled validator still owns its lock until filesystem IO ends.
            let lease = registered(std::ptr::from_ref(self) as usize)?;
            crate::runtime::blocking::spawn_blocking(move || lease.validate_sync(deadline)).await?
        }
    }

    pub(in super::super) async fn acquire(
        pool: Arc<SqliteConnectionPool>,
        database: PathBuf,
        workspace: String,
        token: String,
        deadline: Instant,
        recovery: bool,
    ) -> anyhow::Result<Option<Arc<dyn IngressLifetime>>> {
        anyhow::ensure!(
            !workspace.is_empty() && workspace.len() <= MAX_ID_BYTES,
            "invalid ingress workspace"
        );
        let parsed = uuid::Uuid::parse_str(&token)?;
        anyhow::ensure!(
            parsed.to_string() == token,
            "ingress lifetime must use canonical UUID spelling"
        );
        remaining(deadline)?;
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<Option<Arc<dyn IngressLifetime>>> {
            remaining(deadline)?;
            let namespace = {
                let mut connection = pool.write_until(deadline)?;
                let connection = ConnectionDeadline::new(&mut connection, deadline)?;
                let transaction = begin_immediate(&connection)?;
                let namespace = schema(&transaction)?;
                remaining(deadline)?;
                transaction.commit()?;
                namespace
            };
            let uid = tempfile::tempfile()?.metadata()?.uid();
            // Resolve trusted platform aliases such as macOS /var, but never
            // follow the database leaf or any newly created recovery component.
            let database_parent = database
                .parent()
                .context("ingress database parent missing")?
                .canonicalize()?;
            let database = database_parent.join(database.file_name().context("ingress database name missing")?);
            let workspace_path = Path::new(&workspace).canonicalize()?;
            let workspace_dir = directory(&workspace_path, false, uid)?;
            let database_dir = directory(&database_parent, false, uid)?;
            let database_file = File::from(rustix::fs::open(
                &database,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            check_metadata(&database_file.metadata()?, uid, false, false)?;
            local_filesystem(&workspace_dir.file)?;
            local_filesystem(&database_file)?;
            // A recovery claim never creates: a missing component means the
            // lifetime's lock is missing, which is not a store fault.
            let missing = |error: anyhow::Error| {
                if recovery && error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) {
                    IngressLockMissing.into()
                } else {
                    error
                }
            };
            let recovery_dir = child_directory(&workspace_dir, RECOVERY_DIRECTORY, !recovery, uid).map_err(missing)?;
            let lock_namespace = lock_directory_name(&database, database_file.metadata()?.ino(), &namespace);
            let backend_dir = child_directory(&recovery_dir, &lock_namespace, !recovery, uid).map_err(missing)?;
            let name = format!("{token}.lock");
            let flags = OFlags::RDWR
                | OFlags::NOFOLLOW
                | OFlags::NONBLOCK
                | OFlags::CLOEXEC
                | if recovery {
                    OFlags::empty()
                } else {
                    OFlags::CREATE | OFlags::EXCL
                };
            let lock_file =
                match rustix::fs::openat(&backend_dir.file, name.as_str(), flags, Mode::from_raw_mode(0o600)) {
                    Ok(fd) => File::from(fd),
                    Err(rustix::io::Errno::EXIST) if !recovery => return Ok(None),
                    Err(rustix::io::Errno::NOENT) if recovery => return Err(IngressLockMissing.into()),
                    Err(error) => return Err(error).context("ingress lock cannot be opened safely"),
                };
            check_metadata(&lock_file.metadata()?, uid, false, true)?;
            match FileExt::try_lock_exclusive(&lock_file) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) => return Err(error).context("ingress lifetime locking unavailable"),
            }
            // A crash must not lose the lock pathname after input commits.
            if !recovery {
                lock_file.sync_all()?;
                backend_dir.file.sync_all()?;
            }
            let lock_path = backend_dir.path.join(name);
            let lease = Arc::new(Lease {
                token,
                namespace,
                workspace,
                database,
                database_file,
                directories: vec![workspace_dir, database_dir, recovery_dir, backend_dir],
                lock_path,
                lock_file,
                uid,
                recovery,
            });
            lease.validate_sync(deadline)?;
            let mut registry = issued().lock();
            registry.retain(|_, lease| lease.strong_count() != 0);
            anyhow::ensure!(
                registry.len() < MAX_LIVE_LEASES,
                "SQLite ingress live lease limit reached"
            );
            registry.insert(Arc::as_ptr(&lease) as usize, Arc::downgrade(&lease));
            Ok(Some(lease))
        })
        .await?
    }

    pub(in super::super) fn validate_owner(
        connection: &Connection,
        provided: &Arc<dyn IngressLifetime>,
        workspace: &str,
        deadline: Instant,
    ) -> anyhow::Result<()> {
        remaining(deadline)?;
        let pointer = Arc::as_ptr(provided).cast::<()>() as usize;
        let lease = registered(pointer)?;
        let erased: Arc<dyn IngressLifetime> = lease.clone();
        anyhow::ensure!(
            Arc::ptr_eq(&erased, provided) && !lease.recovery && lease.workspace == workspace,
            "SQLite repair owner identity or scope mismatch"
        );
        let path = Path::new(
            connection
                .path()
                .context("ingress repair requires a file-backed SQLite database")?,
        );
        let actual = path
            .parent()
            .context("ingress repair database parent missing")?
            .canonicalize()?
            .join(path.file_name().context("ingress repair database name missing")?);
        anyhow::ensure!(
            actual == lease.database && schema(connection)? == lease.namespace,
            "SQLite repair owner backend mismatch"
        );
        lease.validate_sync(deadline)
    }

    pub(in super::super) fn validate_lease(
        connection: &Connection,
        operation: &AtomicIngressMutation,
    ) -> anyhow::Result<()> {
        // (recovery lease, the lease is the row's own lifetime)
        let (recovering, owns_row) = match operation {
            AtomicIngressMutation::Input { .. } | AtomicIngressMutation::Owner(_) => (false, true),
            AtomicIngressMutation::Recovery(_) => (true, true),
            AtomicIngressMutation::Orphan(_) => (false, false),
        };
        let provided = operation.lease();
        let pointer = Arc::as_ptr(provided).cast::<()>() as usize;
        let lease = registered(pointer)?;
        let erased: Arc<dyn IngressLifetime> = lease.clone();
        anyhow::ensure!(Arc::ptr_eq(&erased, provided), "ingress lease object mismatch");
        let capsule = operation.capsule();
        anyhow::ensure!(
            (lease.token == capsule.lifetime) == owns_row
                && lease.namespace == capsule.backend_namespace
                && lease.workspace == capsule.scope.workspace_id
                && lease.recovery == recovering,
            "ingress lease purpose or scope mismatch"
        );
        let path = Path::new(
            connection
                .path()
                .context("ingress requires a file-backed SQLite database")?,
        );
        let actual = path
            .parent()
            .context("ingress database parent missing")?
            .canonicalize()?
            .join(path.file_name().context("ingress database name missing")?);
        anyhow::ensure!(actual == lease.database, "ingress lease belongs to another database");
        lease.validate_sync(deadline(operation))?;
        if matches!(operation, AtomicIngressMutation::Orphan(_)) {
            lease.lock_absent(&capsule.lifetime)?;
        }
        Ok(())
    }
}

pub(super) fn validate_event_bounds(event: &MessageEvent) -> anyhow::Result<()> {
    let mut bytes = 0usize;
    let mut add = |value: &str, limit: usize| -> anyhow::Result<()> {
        anyhow::ensure!(value.len() <= limit, "repair field exceeds recovery byte limit");
        bytes = bytes
            .checked_add(value.len())
            .filter(|total| *total <= MAX_RECOVERY_EVENT_BYTES)
            .context("repair event exceeds recovery byte limit")?;
        Ok(())
    };
    for value in [
        event.event_id.as_str(),
        event.workspace_id.as_str(),
        event.source.as_str(),
        event.role.as_str(),
        event.event_type.as_str(),
        event.visibility.as_str(),
        event.created_at.as_str(),
        event.updated_at.as_str(),
    ] {
        add(value, MAX_ID_BYTES)?;
    }
    for value in [
        &event.idempotency_key,
        &event.owner_id,
        &event.channel,
        &event.session_key,
        &event.parent_session_key,
        &event.run_id,
        &event.parent_run_id,
        &event.agent_id,
        &event.persona_id,
        &event.sender,
        &event.recipient,
        &event.goal_id,
        &event.causation_event_id,
        &event.correlation_id,
        &event.attempt_id,
        &event.config_source_revision,
        &event.content_hash,
    ]
    .into_iter()
    .flatten()
    {
        add(value, MAX_ID_BYTES)?;
    }
    use crate::memory::traits::MessageEventSubject;
    if let Some(subject) = &event.subject {
        match subject {
            MessageEventSubject::Project(value)
            | MessageEventSubject::Task(value)
            | MessageEventSubject::Incident(value)
            | MessageEventSubject::Customer(value)
            | MessageEventSubject::Document(value)
            | MessageEventSubject::Goal(value)
            | MessageEventSubject::Topic(value)
            | MessageEventSubject::Conversation(value) => add(value, MAX_ID_BYTES)?,
            MessageEventSubject::Other { subject_type, id } => {
                add(subject_type, MAX_ID_BYTES)?;
                add(id, MAX_ID_BYTES)?;
            }
        }
    }
    add(&event.content, MAX_RECOVERY_EVENT_BYTES)?;
    if let Some(payload) = &event.raw_payload_json {
        add(payload, MAX_RECOVERY_EVENT_BYTES)?;
    }
    serde_json::to_writer(
        EncodedByteLimit {
            remaining: MAX_RECOVERY_EVENT_BYTES,
        },
        event,
    )?;
    Ok(())
}

pub(super) fn check_console_projection_size(connection: &Connection, event_id: &str) -> anyhow::Result<()> {
    let size: Option<i64> = connection.query_row(
        "SELECT MAX(COALESCE(length(CAST(session_key AS BLOB)), 0)
            + COALESCE(length(CAST(owner_id AS BLOB)), 0) + COALESCE(length(CAST(role AS BLOB)), 0)
            + COALESCE(length(CAST(content AS BLOB)), 0))
         FROM (SELECT session_key, owner_id, role, content FROM conversation_turns
            WHERE message_event_id = ?1 OR message_id = ?1 LIMIT 2)",
        [event_id],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        size.is_none_or(|size| (0..=MAX_RECOVERY_EVENT_BYTES as i64).contains(&size)),
        "console projection exceeds recovery byte limit"
    );
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn validate_owner(
    _connection: &Connection,
    _owner: &Arc<dyn IngressLifetime>,
    _workspace: &str,
    _deadline: Instant,
) -> anyhow::Result<()> {
    anyhow::bail!("SQLite ingress repair requires supported local Unix file locks")
}

/// Read-only inspection for `prx doctor ingress`. Unix only: other systems
/// have no SQLite ingress locks, so no ingress row can exist there.
#[cfg(unix)]
mod inspect {
    use super::{Connection, Context, IngressRecoveryState, OrphanCause};

    /// Reachability of one row's lifetime lock, as `prx doctor ingress` reports it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum IngressLockReach {
        /// The lock exists and a live lifetime holds it.
        Held,
        /// The lock exists and nothing holds it: its lifetime ended.
        Present,
        /// Neither the lock file nor its directory exists.
        Missing,
    }

    impl IngressLockReach {
        pub(crate) const fn as_str(self) -> &'static str {
            match self {
                Self::Held => "held",
                Self::Present => "present",
                Self::Missing => "missing",
            }
        }
    }

    /// One row that is not reconciled. It carries identifiers only, never input content.
    #[derive(Debug, Clone)]
    pub(crate) struct UnsettledIngressRow {
        pub(crate) workspace_id: String,
        pub(crate) owner_id: String,
        pub(crate) source: String,
        pub(crate) run_id: String,
        pub(crate) state: IngressRecoveryState,
        pub(crate) received_at: Option<chrono::DateTime<chrono::Utc>>,
        pub(crate) lock: IngressLockReach,
        pub(crate) cause: Option<OrphanCause>,
    }

    /// The rows one inspection reads; `truncated` says more exist.
    #[derive(Debug, Default)]
    pub(crate) struct IngressInspection {
        pub(crate) rows: Vec<UnsettledIngressRow>,
        pub(crate) truncated: bool,
    }

    /// Most rows one inspection reports.
    pub(crate) const MAX_INSPECTED_ROWS: usize = 1000;

    /// List every non-reconciled ingress row of the store at `database`.
    ///
    /// Read-only: the database is opened read-only, and no table, lock directory
    /// or lock file is created. A lock is probed with a non-blocking shared lock
    /// that is released before the next row. A store without the ingress tables
    /// has no rows.
    pub(crate) fn inspect_unsettled_ingress(database: &std::path::Path) -> anyhow::Result<IngressInspection> {
        if !database.is_file() {
            return Ok(IngressInspection::default());
        }
        let canonical = database
            .parent()
            .context("ingress database parent missing")?
            .canonicalize()?
            .join(database.file_name().context("ingress database name missing")?);
        let connection = Connection::open_with_flags(&canonical, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let tables: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table'
                AND name IN ('ingress_recovery_schema', 'ingress_recovery')",
            [],
            |row| row.get(0),
        )?;
        if tables < 2 {
            return Ok(IngressInspection::default());
        }
        let namespace: String = connection.query_row(
            "SELECT backend_namespace FROM ingress_recovery_schema WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(
            "SELECT substr(r.workspace_id, 1, 4096), substr(r.owner_id, 1, 4096), substr(r.source, 1, 4096), r.state,
                    substr(r.lifetime, 1, 36), substr(json_extract(r.capsule_json, '$.run_id'), 1, 4096),
                    substr(e.created_at, 1, 64), substr(json_extract(v.raw_payload_json, '$.ingress_recovery.cause'), 1, 64)
             FROM ingress_recovery r
             LEFT JOIN message_events e ON e.event_id = r.input_event_id
             LEFT JOIN message_events v ON v.event_id = r.evidence_event_id
             WHERE r.state <> 'reconciled'
             ORDER BY r.cursor LIMIT ?1",
        )?;
        let mut rows = statement.query([i64::try_from(MAX_INSPECTED_ROWS + 1)?])?;
        let mut inspection = IngressInspection::default();
        while let Some(row) = rows.next()? {
            if inspection.rows.len() == MAX_INSPECTED_ROWS {
                inspection.truncated = true;
                break;
            }
            let workspace: String = row.get(0)?;
            let state: String = row.get(3)?;
            let lifetime: String = row.get(4)?;
            let received: Option<String> = row.get(6)?;
            let cause: Option<String> = row.get(7)?;
            inspection.rows.push(UnsettledIngressRow {
                owner_id: row.get(1)?,
                source: row.get(2)?,
                run_id: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                state: match state.as_str() {
                    "pending" => IngressRecoveryState::Pending,
                    "unconfirmed" => IngressRecoveryState::Unconfirmed,
                    _ => anyhow::bail!("unknown ingress recovery state"),
                },
                received_at: received
                    .and_then(|text| chrono::DateTime::parse_from_rfc3339(&text).ok())
                    .map(|time| time.with_timezone(&chrono::Utc)),
                lock: probe_lock(&workspace, &canonical, &namespace, &lifetime)?,
                cause: cause.as_deref().and_then(OrphanCause::from_code),
                workspace_id: workspace,
            });
        }
        Ok(inspection)
    }

    fn probe_lock(
        workspace: &str,
        database: &std::path::Path,
        namespace: &str,
        token: &str,
    ) -> anyhow::Result<IngressLockReach> {
        use fs2::FileExt;
        use rustix::fs::{Mode, OFlags};
        use std::os::unix::fs::MetadataExt;
        anyhow::ensure!(
            uuid::Uuid::parse_str(token)?.to_string() == token,
            "ingress lifetime is not canonical"
        );
        let workspace = match std::path::Path::new(workspace).canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(IngressLockReach::Missing),
            Err(error) => return Err(error).context("ingress workspace cannot be inspected"),
        };
        let path = workspace
            .join(super::local::RECOVERY_DIRECTORY)
            .join(super::local::lock_directory_name(
                database,
                std::fs::metadata(database)?.ino(),
                namespace,
            ))
            .join(format!("{token}.lock"));
        let file = match rustix::fs::open(
            &path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => std::fs::File::from(fd),
            Err(rustix::io::Errno::NOENT) => return Ok(IngressLockReach::Missing),
            Err(error) => return Err(error).context("ingress lock cannot be inspected"),
        };
        match FileExt::try_lock_shared(&file) {
            Ok(()) => {
                FileExt::unlock(&file)?;
                Ok(IngressLockReach::Present)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(IngressLockReach::Held),
            Err(error) => Err(error).context("ingress lock cannot be inspected"),
        }
    }
}

#[cfg(unix)]
pub(crate) use inspect::{IngressLockReach, MAX_INSPECTED_ROWS, UnsettledIngressRow, inspect_unsettled_ingress};

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct ResetDeadlineGuard;

    impl Drop for ResetDeadlineGuard {
        fn drop(&mut self) {
            IO_DEADLINE.with(|cell| cell.set(None));
            FAIL_HANDLER_STAGE.with(|cell| cell.set(None));
            DEADLINE_BUSY_INSTALLED.with(|cell| cell.set(false));
        }
    }

    /// I1: the lock directory is named from the canonical database path, its
    /// inode and the backend namespace only. The real lock of a real lifetime
    /// lives at exactly that path, so a renumbered device (a reboot) still
    /// reaches every lock.
    #[cfg(unix)]
    #[tokio::test]
    async fn lock_path_is_independent_of_st_dev() {
        use crate::memory::Memory as _;
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().expect("test: workspace");
        let memory = SqliteMemory::new(dir.path()).expect("test: store");
        let deadline = Instant::now() + Duration::from_secs(10);
        let token = uuid::Uuid::new_v4().to_string();
        let lifetime = memory
            .acquire_ingress_lifetime(&dir.path().to_string_lossy(), &token, deadline)
            .await
            .expect("test: lifetime");
        let database = dir
            .path()
            .join("memory")
            .canonicalize()
            .expect("test: database directory")
            .join("brain.db");
        let inode = std::fs::metadata(&database).expect("test: database metadata").ino();
        let name = lock_directory_name(&database, inode, lifetime.namespace());
        assert_eq!(
            name,
            SqliteMemory::content_hash(&format!("{}:{inode}:{}", database.display(), lifetime.namespace())),
            "the name hashes path, inode and namespace and nothing else"
        );
        let lock = dir
            .path()
            .canonicalize()
            .expect("test: workspace path")
            .join(RECOVERY_DIRECTORY)
            .join(&name)
            .join(format!("{}.lock", lifetime.token()));
        let metadata = std::fs::symlink_metadata(&lock).expect("test: the real lock lives at the stable path");
        assert!(metadata.is_file());
        // The device number is never part of the name.
        let device = std::fs::metadata(&database).expect("test: database metadata").dev();
        assert_ne!(
            name,
            SqliteMemory::content_hash(&format!(
                "{}:{device}:{inode}:{}",
                database.display(),
                lifetime.namespace()
            ))
        );
    }

    #[test]
    fn connection_deadline_install_failure_restores_previous_deadline() {
        let _reset = ResetDeadlineGuard;
        for stage in [
            HandlerFailStage::BeforeBusy,
            HandlerFailStage::AfterBusy,
            HandlerFailStage::AfterProgress,
        ] {
            let mut connection = Connection::open_in_memory().expect("test: memory db");
            let previous = Instant::now() + Duration::from_secs(9);
            IO_DEADLINE.with(|cell| cell.set(Some(previous)));
            FAIL_HANDLER_STAGE.with(|cell| cell.set(Some(stage)));
            DEADLINE_BUSY_INSTALLED.with(|cell| cell.set(false));
            let deadline = Instant::now() + Duration::from_secs(60);
            assert!(
                ConnectionDeadline::new(&mut connection, deadline).is_err(),
                "test: install must fail at {stage:?}"
            );
            assert_eq!(
                IO_DEADLINE.with(Cell::get),
                Some(previous),
                "test: thread-local deadline restored to previous at {stage:?}"
            );
            assert!(
                !DEADLINE_BUSY_INSTALLED.with(Cell::get),
                "test: deadline busy handler removed at {stage:?}"
            );
        }
    }
}
