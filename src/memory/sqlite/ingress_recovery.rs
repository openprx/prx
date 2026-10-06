//! SQLite's durable ingress projection and locally fenced lifetime ownership.
//! All message/projection mutations share the existing append transaction.
use super::{MessageEvent, MessageEventInput, SqliteConnectionPool, SqliteMemory};
use crate::memory::ingress_recovery::{
    AtomicIngressMutation, IngressCapsule, IngressEventLookup, IngressLifetime, IngressRecoveryCandidate,
    IngressRecoveryScope, IngressRecoveryState, MAX_CAPSULE_BYTES, MAX_ID_BYTES, MAX_RECOVERY_EVENT_BYTES,
    MAX_RECOVERY_PAGE, remaining,
};
use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

thread_local! {
    static IO_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

fn wait_for_lock(_attempt: i32) -> bool {
    let left = IO_DEADLINE
        .with(Cell::get)
        .and_then(|deadline| remaining(deadline).ok());
    left.is_some_and(|left| {
        std::thread::sleep(left.min(Duration::from_millis(5)));
        IO_DEADLINE
            .with(Cell::get)
            .is_some_and(|deadline| Instant::now() < deadline)
    })
}

/// Restores the pool's normal handler before its writer is returned. Rollback
/// drops before this guard, so even failed transactions retain the deadline.
pub(super) struct ConnectionDeadline<'a> {
    connection: &'a Connection,
    previous: Option<Instant>,
}
impl<'a> ConnectionDeadline<'a> {
    pub(super) fn new(connection: &'a Connection, deadline: Instant) -> anyhow::Result<Self> {
        remaining(deadline)?;
        let previous = IO_DEADLINE.with(|cell| cell.replace(Some(deadline)));
        let guard = Self { connection, previous };
        connection.busy_handler(Some(wait_for_lock))?;
        connection.progress_handler(1_000, Some(move || Instant::now() >= deadline))?;
        Ok(guard)
    }
}
impl Drop for ConnectionDeadline<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.connection.progress_handler(0, None::<fn() -> bool>) {
            tracing::error!(%error, "SQLite ingress progress handler reset failed");
        }
        if let Err(error) = crate::runtime::sqlite_pool::install_busy_handler(self.connection) {
            tracing::error!(%error, "SQLite ingress busy handler reset failed");
        }
        IO_DEADLINE.with(|cell| cell.set(self.previous));
    }
}

pub(super) const fn deadline(operation: &AtomicIngressMutation) -> Instant {
    match operation {
        AtomicIngressMutation::Input { deadline, .. } => *deadline,
        AtomicIngressMutation::Recovery(commit) => commit.deadline,
    }
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
fn schema(connection: &Connection) -> anyhow::Result<String> {
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
    anyhow::ensure!(
        uuid::Uuid::parse_str(&namespace)?.to_string() == namespace,
        "invalid SQLite ingress backend namespace"
    );
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

pub(super) fn before_append(
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
        AtomicIngressMutation::Recovery(commit) => {
            anyhow::ensure!(
                encode_capsule(&stored_capsule(transaction, &capsule.input_event_id)?)? == encode_capsule(capsule)?,
                "recovery capsule differs from durable input"
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
                        .is_some_and(|mac| mac.len() == 64 && mac.bytes().all(|byte| byte.is_ascii_hexdigit())),
                "recovery evidence binding mismatch"
            );
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

pub(super) fn after_append(
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
                        source, input_key, lifetime, capsule_json, state)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')",
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
        AtomicIngressMutation::Recovery(commit) => {
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
        let connection = pool.write_until(deadline)?;
        let _budget = ConnectionDeadline::new(&connection, deadline)?;
        let transaction = connection.unchecked_transaction()?;
        let namespace = schema(&transaction)?;
        validate_owner(&transaction,&current,&scope.workspace_id,deadline)?;
        let other_state=match phase {crate::memory::ingress_recovery::IngressRecoveryScan::PendingOnly=>"pending",crate::memory::ingress_recovery::IngressRecoveryScan::IncludeUnconfirmed=>"unconfirmed"};
        let candidates = {
            let projection="cursor, CASE WHEN length(CAST(capsule_json AS BLOB)) <= ?6 THEN capsule_json ELSE NULL END,
                CASE WHEN length(CAST(input_event_id AS BLOB))<=4096 THEN input_event_id ELSE NULL END,
                CASE WHEN length(CAST(input_key AS BLOB))<=4096 THEN input_key ELSE NULL END,
                CASE WHEN length(CAST(lifetime AS BLOB))<=36 THEN lifetime ELSE NULL END";
            // Two explicit indexed lifetime ranges exclude the entire current
            // owner's history before materialization or cursor ordering.
            let sql=format!("SELECT * FROM (
                SELECT {projection} FROM ingress_recovery INDEXED BY ingress_recovery_scope_cursor WHERE workspace_id=?1 AND owner_id=?2 AND source=?3 AND state IN ('pending',?8) AND state<>'reconciled' AND lifetime<?7 AND cursor>?4
                UNION ALL
                SELECT {projection} FROM ingress_recovery INDEXED BY ingress_recovery_scope_cursor WHERE workspace_id=?1 AND owner_id=?2 AND source=?3 AND state IN ('pending',?8) AND state<>'reconciled' AND lifetime>?7 AND cursor>?4
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
        let connection = pool.write_until(deadline)?;
        let _budget = ConnectionDeadline::new(&connection, deadline)?;
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

#[cfg(unix)]
pub(super) use local::{acquire, validate_lease, validate_owner};

#[cfg(unix)]
mod local {
    use super::{
        Arc, AtomicIngressMutation, Connection, ConnectionDeadline, Context, IngressLifetime, Instant, MAX_ID_BYTES,
        PathBuf, SqliteConnectionPool, SqliteMemory, deadline, remaining, schema,
    };
    use fs2::FileExt;
    use parking_lot::Mutex;
    use rustix::fs::{Mode, OFlags};
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

    /// Only known local filesystems are admitted. NFS/CIFS/FUSE and unknown
    /// types cannot supply this single-host lifetime proof.
    fn local_filesystem(file: &File) -> anyhow::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let stat = rustix::fs::fstatfs(file)?;
            let kind = stat.f_type as u64;
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
                let connection = pool.write_until(deadline)?;
                let _budget = ConnectionDeadline::new(&connection, deadline)?;
                let transaction = connection.unchecked_transaction()?;
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
            let recovery_dir = child_directory(&workspace_dir, ".prx-ingress-recovery-v1", !recovery, uid)?;
            // The namespace includes physical backend identity. Separate DBs
            // in one workspace cannot accidentally arbitrate each other's runs.
            let metadata = database_file.metadata()?;
            let lock_namespace = SqliteMemory::content_hash(&format!(
                "{}:{}:{}:{namespace}",
                database.display(),
                metadata.dev(),
                metadata.ino()
            ));
            let backend_dir = child_directory(&recovery_dir, &lock_namespace, !recovery, uid)?;
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
        let (provided, recovering) = match operation {
            AtomicIngressMutation::Input { lifetime, .. } => (lifetime, false),
            AtomicIngressMutation::Recovery(commit) => (&commit.claim.lifetime, true),
        };
        let pointer = Arc::as_ptr(provided).cast::<()>() as usize;
        let lease = registered(pointer)?;
        let erased: Arc<dyn IngressLifetime> = lease.clone();
        anyhow::ensure!(Arc::ptr_eq(&erased, provided), "ingress lease object mismatch");
        let capsule = operation.capsule();
        anyhow::ensure!(
            lease.token == capsule.lifetime
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
        lease.validate_sync(deadline(operation))
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
