//! Stream identity and scope rebinds; wire UUIDs never reset local rowids.
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use super::{
    SqliteMailbox,
    model::{Progress, decode, encode, uuid},
    pin::{check_scope, scope_from_row},
    storage_error,
};
use crate::messaging::mailbox::{MAX_ROWS, MailboxError, MailboxResult, MailboxScope, validate_address};
use crate::messaging::{PrxMessage, SessionAddr};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Sender,
    Recipient,
}

pub(super) struct Stream {
    pub id: i64,
    pub workspace_id: String,
    pub owner_id: String,
    pub from: SessionAddr,
    pub to: SessionAddr,
    pub wire_id: Uuid,
    pub sender: Option<MailboxScope>,
    pub recipient: Option<MailboxScope>,
    pub retired: bool,
    pub progress: Progress,
}

fn role_scope(
    workspace: &str,
    owner: &str,
    scope: Option<String>,
    inc: Option<String>,
    generation: Option<i64>,
) -> MailboxResult<Option<MailboxScope>> {
    match (scope, inc, generation) {
        (None, None, None) => Ok(None),
        (Some(id), Some(inc), Some(generation)) => Ok(Some(scope_from_row(
            workspace.to_owned(),
            owner.to_owned(),
            id,
            inc,
            generation,
        )?)),
        _ => Err(MailboxError::CorruptState),
    }
}

pub(super) fn load(conn: &Connection, id: i64) -> MailboxResult<Stream> {
    let raw=conn.query_row(
        "SELECT workspace_id,owner_id,from_addr,to_addr,wire_stream_id,state,progress,
         sender_ma_scope_id,sender_incarnation,sender_generation,recipient_ma_scope_id,recipient_incarnation,recipient_generation
         FROM prx_mailbox_seq WHERE stream_id=?1",[id],
        |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Vec<u8>>(4)?,r.get::<_,i64>(5)?,r.get::<_,String>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,Option<String>>(8)?,r.get::<_,Option<i64>>(9)?,r.get::<_,Option<String>>(10)?,r.get::<_,Option<String>>(11)?,r.get::<_,Option<i64>>(12)?))
    ).map_err(storage_error)?;
    let from: SessionAddr = decode(&raw.2)?;
    let to: SessionAddr = decode(&raw.3)?;
    validate_address(&from)?;
    validate_address(&to)?;
    if encode(&from)? != raw.2 || encode(&to)? != raw.3 || !matches!(raw.5, 0 | 1) {
        return Err(MailboxError::CorruptState);
    }
    let progress: Progress = decode(&raw.6)?;
    progress.validate(raw.5 == 1)?;
    let sender = role_scope(&raw.0, &raw.1, raw.7, raw.8, raw.9)?;
    let recipient = role_scope(&raw.0, &raw.1, raw.10, raw.11, raw.12)?;
    if sender.is_none() && recipient.is_none() {
        return Err(MailboxError::CorruptState);
    }
    if let Some(scope) = &sender {
        check_scope(conn, scope, &from, None, false)?;
    }
    if let Some(scope) = &recipient {
        check_scope(conn, scope, &to, None, false)?;
    }
    Ok(Stream {
        id,
        workspace_id: raw.0,
        owner_id: raw.1,
        from,
        to,
        wire_id: uuid(&raw.4)?,
        sender,
        recipient,
        retired: raw.5 == 1,
        progress,
    })
}

pub(super) fn save_progress(conn: &Connection, stream: &Stream, now: i64) -> MailboxResult<()> {
    stream.progress.validate(stream.retired)?;
    conn.execute(
        "UPDATE prx_mailbox_seq SET progress=?1,state=?2,updated_at=?3 WHERE stream_id=?4",
        params![encode(&stream.progress)?, i64::from(stream.retired), now, stream.id],
    )
    .map_err(storage_error)?;
    Ok(())
}

pub(super) fn retire(conn: &Connection, stream: &mut Stream, now: i64) -> MailboxResult<()> {
    stream.retired = true;
    stream.progress.retired_at.get_or_insert(now);
    save_progress(conn, stream, now)
}

impl SqliteMailbox {
    pub(super) fn ensure_stream(
        &self,
        conn: &Connection,
        message: &PrxMessage,
        scope: &MailboxScope,
        role: Role,
    ) -> MailboxResult<Stream> {
        let now = self.clock.now();
        let wire = message.stream_id.ok_or(MailboxError::InvalidMetadata)?;
        let from = encode(&message.from)?;
        let to = encode(&message.to)?;
        let found=conn.query_row(
            "SELECT stream_id FROM prx_mailbox_seq WHERE workspace_id=?1 AND owner_id=?2 AND from_addr=?3 AND to_addr=?4 AND wire_stream_id=?5",
            params![scope.workspace_id,scope.owner_id,from,to,wire.as_bytes().as_slice()],|r|r.get::<_,i64>(0)
        ).optional().map_err(storage_error)?;
        let id = if let Some(id) = found {
            id
        } else {
            let count_rows = || -> MailboxResult<i64> {
                conn.query_row("SELECT (SELECT count(*) FROM prx_mailbox_seq WHERE workspace_id=?1 AND owner_id=?2)+(SELECT count(*) FROM prx_mailbox_parents WHERE workspace_id=?1 AND owner_id=?2)",params![scope.workspace_id,scope.owner_id],|r|r.get(0)).map_err(storage_error)
            };
            if count_rows()? >= MAX_ROWS {
                self.gc_owner_rows(conn, &scope.workspace_id, &scope.owner_id)?;
            }
            if count_rows()? >= MAX_ROWS {
                return Err(MailboxError::Quota("streams"));
            }
            let sender = (role == Role::Sender).then_some(scope);
            let recipient = (role == Role::Recipient).then_some(scope);
            conn.execute(
                "INSERT INTO prx_mailbox_seq(workspace_id,owner_id,sender_ma_scope_id,sender_incarnation,sender_generation,recipient_ma_scope_id,recipient_incarnation,recipient_generation,from_addr,to_addr,wire_stream_id,state,progress,updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,?12,?13)",
                params![scope.workspace_id,scope.owner_id,sender.map(|s|s.scope_id.to_string()),sender.map(|s|s.incarnation.to_string()),sender.map(|s|s.generation),recipient.map(|s|s.scope_id.to_string()),recipient.map(|s|s.incarnation.to_string()),recipient.map(|s|s.generation),from,to,wire.as_bytes().as_slice(),encode(&Progress::new(now))?,now]
            ).map_err(storage_error)?;
            conn.last_insert_rowid()
        };
        let mut stream = load(conn, id)?;
        if stream.retired {
            return Ok(stream);
        }
        let address = if role == Role::Sender { &stream.from } else { &stream.to };
        if address.instance != self.local_instance || scope.owner_id != address.owner {
            return Err(MailboxError::Unauthorized);
        }
        check_scope(conn, scope, address, None, true)?;
        let previous = if role == Role::Sender {
            &stream.sender
        } else {
            &stream.recipient
        };
        if previous.as_ref() == Some(scope) {
            return Ok(stream);
        }
        if role == Role::Recipient {
            // All historical scopes, not only the current role: an old writer
            // could have rebound it while leaving a pending X-pinned receipt.
            let blocked: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM prx_mailbox_receipts WHERE ma_scope_id IN
                 (SELECT scope_id FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2)
                 AND stream_id=?3 AND state<>2 AND NOT(ma_scope_id=?4 AND incarnation=?5 AND generation=?6))",
                    params![
                        scope.workspace_id,
                        scope.owner_id,
                        id,
                        scope.scope_id.to_string(),
                        scope.incarnation.to_string(),
                        scope.generation
                    ],
                    |r| r.get(0),
                )
                .map_err(storage_error)?;
            if blocked {
                retire(conn, &mut stream, now)?;
                return Ok(stream);
            }
            if previous
                .as_ref()
                .is_some_and(|old| self.memory.mailbox_runtime.has_scope_leases(old.scope_id))
            {
                return Err(MailboxError::RoleBusy);
            }
            conn.execute("UPDATE prx_mailbox_seq SET recipient_ma_scope_id=?1,recipient_incarnation=?2,recipient_generation=?3 WHERE stream_id=?4",params![scope.scope_id.to_string(),scope.incarnation.to_string(),scope.generation,id]).map_err(storage_error)?;
        } else {
            let blocked:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM prx_mailbox_outbox WHERE ma_scope_id IN(SELECT scope_id FROM agent_scopes WHERE workspace_id=?1 AND owner_id=?2) AND stream_id=?3 AND state IN (0,1) AND NOT(ma_scope_id=?4 AND incarnation=?5 AND generation=?6))",params![scope.workspace_id,scope.owner_id,id,scope.scope_id.to_string(),scope.incarnation.to_string(),scope.generation],|r|r.get(0)).map_err(storage_error)?;
            if blocked {
                return Err(MailboxError::RoleBusy);
            }
            if previous
                .as_ref()
                .is_some_and(|old| self.memory.mailbox_runtime.has_scope_leases(old.scope_id))
            {
                return Err(MailboxError::RoleBusy);
            }
            conn.execute("UPDATE prx_mailbox_seq SET sender_ma_scope_id=?1,sender_incarnation=?2,sender_generation=?3 WHERE stream_id=?4",params![scope.scope_id.to_string(),scope.incarnation.to_string(),scope.generation,id]).map_err(storage_error)?;
        }
        load(conn, id)
    }
}
