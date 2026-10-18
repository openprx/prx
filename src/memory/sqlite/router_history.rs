//! Exact-domain local Router history; no generic memory/category scan.
use super::SqliteMemory;
use crate::memory::router_history::{
    ROUTER_HISTORY_MAX_RECORDS, RouterHistoryCommitReceipt, RouterHistoryDomain, RouterHistoryRead,
    RouterHistoryReadEvidence, RouterHistoryRow, RouterHistoryWriteEvidence, decode_vector,
};
use crate::memory::traits::RouterLearningEvidence;
use anyhow::Context;
use rusqlite::{OptionalExtension, Transaction, params};
use std::sync::Arc;
use std::time::Instant;

fn check(deadline: Instant) -> anyhow::Result<()> {
    anyhow::ensure!(Instant::now() < deadline, "Router history deadline expired");
    Ok(())
}

fn schema(tx: &Transaction<'_>) -> anyhow::Result<()> {
    // The row is also the permanent receipt ledger. Capacity never evicts it.
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS router_history_v1 (
        domain TEXT NOT NULL, workspace TEXT NOT NULL, owner TEXT NOT NULL,
        session TEXT NOT NULL, policy BLOB NOT NULL CHECK(length(policy)=32),
        embedding_model TEXT NOT NULL, dimensions INTEGER NOT NULL CHECK(dimensions BETWEEN 1 AND 2048),
        embedding_version INTEGER NOT NULL CHECK(embedding_version=1),
        source_event TEXT NOT NULL, source_sequence INTEGER NOT NULL,
        terminal TEXT NOT NULL, terminal_sequence INTEGER NOT NULL,
        provider TEXT NOT NULL, model TEXT NOT NULL, success INTEGER NOT NULL CHECK(success IN (0,1)),
        embedding BLOB NOT NULL CHECK(length(embedding)=dimensions*4),
        fingerprint BLOB NOT NULL CHECK(length(fingerprint)=32),
        PRIMARY KEY(workspace,owner,terminal), CHECK(source_sequence>0 AND terminal_sequence>source_sequence));
        CREATE INDEX IF NOT EXISTS router_history_domain_sequence_v1
        ON router_history_v1(domain,source_sequence,terminal_sequence);",
    )?;
    Ok(())
}

fn source_sequence(tx: &Transaction<'_>, evidence: &RouterHistoryReadEvidence) -> anyhow::Result<i64> {
    let source = &evidence.source;
    let domain = &evidence.domain;
    let value: Option<i64> = tx
        .query_row(
            "SELECT id FROM message_events WHERE event_id=?1
        AND workspace_id=?2 AND owner_id=?3 AND session_key=?4 AND run_id=?5
        AND source='agent' AND source_ref_json=?6 AND channel='agent_api' AND sender='embedded-caller'
        AND role='user' AND event_type='message.created' AND idempotency_key=('agent_api:' || ?5 || ':input')
        AND content=?7 AND content_hash=?8 AND raw_payload_json IS ?9
        AND config_generation_id=?10 AND config_source_revision=?11
        AND visibility='session' AND recipient=session_key
        AND parent_session_key IS NULL AND parent_run_id IS NULL AND agent_id IS NULL AND persona_id IS NULL
        AND subject_ref_json IS NULL AND goal_id IS NULL AND causation_event_id IS NULL AND correlation_id IS NULL
        AND attempt_id IS NULL AND lease_epoch IS NULL",
            params![
                source.event_id,
                domain.workspace,
                domain.owner,
                domain.session,
                source.run,
                serde_json::to_string(&crate::memory::MessageEventSource::Agent)?,
                source.input,
                SqliteMemory::content_hash(&source.input),
                source.raw_payload,
                source.generation,
                source.revision
            ],
            |row| row.get(0),
        )
        .optional()?;
    let id = value.context("Router history current source authority mismatch")?;
    anyhow::ensure!(id > 0, "Router history source sequence invalid");
    Ok(id)
}

// CASE guards every variable-width column before rusqlite materializes it.
const COLUMNS: &str = "source_sequence,terminal_sequence,
    CASE WHEN length(CAST(terminal AS BLOB)) BETWEEN 1 AND 512 THEN terminal END,
    CASE WHEN length(CAST(provider AS BLOB)) BETWEEN 1 AND 512 THEN provider END,
    CASE WHEN length(CAST(model AS BLOB)) BETWEEN 1 AND 512 THEN model END,success,
    CASE WHEN typeof(embedding)='blob' AND length(embedding)<=8192 THEN embedding END,
    CASE WHEN length(CAST(workspace AS BLOB)) BETWEEN 1 AND 512 THEN workspace END,
    CASE WHEN length(CAST(owner AS BLOB)) BETWEEN 1 AND 512 THEN owner END,
    CASE WHEN length(CAST(session AS BLOB)) BETWEEN 1 AND 512 THEN session END,
    CASE WHEN typeof(policy)='blob' AND length(policy)=32 THEN policy END,
    CASE WHEN length(CAST(embedding_model AS BLOB)) BETWEEN 1 AND 512 THEN embedding_model END,
    dimensions,embedding_version,
    CASE WHEN typeof(fingerprint)='blob' AND length(fingerprint)=32 THEN fingerprint END,
    CASE WHEN length(CAST(source_event AS BLOB)) BETWEEN 1 AND 512 THEN source_event END";

fn decode(
    row: &rusqlite::Row<'_>,
    domain: &RouterHistoryDomain,
) -> anyhow::Result<(RouterHistoryRow, [u8; 32], String)> {
    let string = |index| -> anyhow::Result<&str> {
        row.get_ref(index)?
            .as_str()
            .context("Router history metadata exceeds bounds")
    };
    let bytes = |index| -> anyhow::Result<&[u8]> {
        row.get_ref(index)?
            .as_blob()
            .context("Router history binary value exceeds bounds")
    };
    anyhow::ensure!(
        string(7)? == domain.workspace
            && string(8)? == domain.owner
            && string(9)? == domain.session
            && bytes(10)? == domain.policy_sha256.as_slice()
            && string(11)? == domain.embedding_model
            && row.get::<_, i64>(12)? == i64::from(domain.dimensions)
            && row.get::<_, i64>(13)? == i64::from(domain.embedding_version),
        "Router history stored domain conflicts"
    );
    let success = row.get::<_, i64>(5)?;
    anyhow::ensure!(matches!(success, 0 | 1), "invalid Router history outcome");
    let fingerprint = bytes(14)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid Router history receipt"))?;
    let result = RouterHistoryRow {
        source_sequence: row.get(0)?,
        terminal_sequence: row.get(1)?,
        terminal_event_id: string(2)?.to_owned(),
        provider: string(3)?.to_owned(),
        model: string(4)?.to_owned(),
        success: success == 1,
        embedding: decode_vector(bytes(6)?, domain.dimensions)?,
    };
    Ok((result, fingerprint, string(15)?.to_owned()))
}

impl SqliteMemory {
    pub(super) async fn router_history_read(
        &self,
        evidence: RouterHistoryReadEvidence,
        deadline: Instant,
    ) -> anyhow::Result<RouterHistoryRead> {
        evidence.validate(deadline)?;
        let pool = Arc::clone(&self.pool);
        crate::runtime::blocking::spawn_blocking(move || {
            check(deadline)?;
            let connection=pool.write_until(deadline)?;
            let _deadline=super::ingress_recovery::ConnectionDeadline::new(&connection,deadline)?;
            let tx=connection.unchecked_transaction()?;
            let sequence=source_sequence(&tx,&evidence)?;
            // First selected read may observe an empty new history. It does not
            // create either a mutable record or a history table.
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='router_history_v1')",[],|row|row.get(0))?;
            let mut result=RouterHistoryRead {source_sequence:sequence,rows:Vec::new()};
            if exists {
                let key=evidence.domain.key()?;
                let (count,valid):(i64,i64)=tx.query_row("SELECT count(*),coalesce(min(CASE WHEN
                    length(CAST(terminal AS BLOB)) BETWEEN 1 AND 512 AND length(CAST(provider AS BLOB)) BETWEEN 1 AND 512
                    AND length(CAST(model AS BLOB)) BETWEEN 1 AND 512 AND length(CAST(workspace AS BLOB)) BETWEEN 1 AND 512
                    AND length(CAST(owner AS BLOB)) BETWEEN 1 AND 512 AND length(CAST(session AS BLOB)) BETWEEN 1 AND 512
                    AND length(CAST(embedding_model AS BLOB)) BETWEEN 1 AND 512 AND length(CAST(source_event AS BLOB)) BETWEEN 1 AND 512
                    AND typeof(policy)='blob' AND length(policy)=32 AND typeof(fingerprint)='blob' AND length(fingerprint)=32
                    AND dimensions BETWEEN 1 AND 2048 AND embedding_version=1 AND success IN (0,1)
                    AND typeof(embedding)='blob' AND length(embedding)=dimensions*4
                    THEN 1 ELSE 0 END),1) FROM (SELECT * FROM router_history_v1
                    WHERE domain=?1 AND source_sequence<?2 AND terminal_sequence<?2 LIMIT ?3)",
                    params![key,sequence,(ROUTER_HISTORY_MAX_RECORDS+1) as i64],|row|Ok((row.get(0)?,row.get(1)?)))?;
                anyhow::ensure!(count<=ROUTER_HISTORY_MAX_RECORDS as i64,"Router history complete domain exceeds capacity");
                anyhow::ensure!(valid==1,"Router history row exceeds encoding bounds");
                check(deadline)?;
                let mut statement=tx.prepare(&format!("SELECT {COLUMNS} FROM router_history_v1
                    WHERE domain=?1 AND source_sequence<?2 AND terminal_sequence<?2
                    ORDER BY source_sequence,terminal_sequence LIMIT ?3"))?;
                let mut rows=statement.query(params![key,sequence,(ROUTER_HISTORY_MAX_RECORDS+1) as i64])?;
                while let Some(row)=rows.next()? {
                    check(deadline)?;
                    anyhow::ensure!(result.rows.len()<ROUTER_HISTORY_MAX_RECORDS,"Router history complete domain exceeds capacity");
                    result.rows.push(decode(row,&evidence.domain)?.0);
                }
            }
            result.validate(&evidence.domain)?;check(deadline)?;tx.commit()?;Ok(result)
        }).await?
    }

    pub(super) async fn router_history_commit(
        &self,
        evidence: &RouterHistoryWriteEvidence,
        deadline: Instant,
    ) -> anyhow::Result<RouterHistoryCommitReceipt> {
        check(deadline)?;
        evidence.validate()?;
        let fingerprint = evidence.fingerprint()?;
        let vector = evidence.vector_bytes()?;
        let domain = evidence.domain.clone();
        let key = domain.key()?;
        let evidence = evidence.terminal.clone();
        let pool = Arc::clone(&self.pool);
        crate::runtime::blocking::spawn_blocking(move || {
            check(deadline)?;
            let connection=pool.write_until(deadline)?;
            let _deadline=super::ingress_recovery::ConnectionDeadline::new(&connection,deadline)?;
            let tx=Transaction::new_unchecked(&connection,rusqlite::TransactionBehavior::Immediate)?;
            let source_sequence=validate_terminal(&tx,&evidence)?;
            let event=&evidence.terminal;
            schema(&tx)?;
            let replay={
                let mut statement=tx.prepare(&format!("SELECT {COLUMNS},CASE WHEN length(domain)=64 THEN domain END
                    FROM router_history_v1 WHERE workspace=?1 AND owner=?2 AND terminal=?3"))?;
                let mut rows=statement.query(params![domain.workspace,domain.owner,event.event_id])?;
                if let Some(row)=rows.next()? {
                    let (stored,digest,source)=decode(row,&domain)?;
                    anyhow::ensure!(row.get::<_,Option<String>>(16)?.as_deref()==Some(key.as_str())
                        && digest==fingerprint && source==evidence.source_event_id
                        && stored.source_sequence==source_sequence && stored.terminal_sequence==event.id
                        && stored.provider==evidence.provider && stored.model==evidence.model && stored.success==evidence.success
                        && stored.embedding.iter().flat_map(|value|value.to_le_bytes()).eq(vector.iter().copied()),
                        "Router history terminal replay conflicts");
                    Some(digest)
                } else {None}
            };
            if let Some(record_sha256)=replay {
                check(deadline)?;tx.commit()?;
                return Ok(RouterHistoryCommitReceipt {terminal_event_id:event.event_id.clone(),record_sha256,replayed:true});
            }
            let count:i64=tx.query_row("SELECT count(*) FROM (SELECT 1 FROM router_history_v1 WHERE domain=?1 LIMIT ?2)",
                params![key,(ROUTER_HISTORY_MAX_RECORDS+1) as i64],|row|row.get(0))?;
            anyhow::ensure!(count<(ROUTER_HISTORY_MAX_RECORDS as i64),"Router history complete domain capacity exhausted; receipt ledger retained");
            tx.execute("INSERT INTO router_history_v1(domain,workspace,owner,session,policy,embedding_model,dimensions,embedding_version,
                source_event,source_sequence,terminal,terminal_sequence,provider,model,success,embedding,fingerprint)
                VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                params![key,domain.workspace,domain.owner,domain.session,domain.policy_sha256.as_slice(),domain.embedding_model,
                    i64::from(domain.dimensions),i64::from(domain.embedding_version),evidence.source_event_id,source_sequence,
                    event.event_id,event.id,evidence.provider,evidence.model,i64::from(evidence.success),vector,fingerprint.as_slice()])?;
            let stored:Vec<u8>=tx.query_row("SELECT CASE WHEN length(fingerprint)=32 THEN fingerprint END FROM router_history_v1
                WHERE workspace=?1 AND owner=?2 AND terminal=?3",params![domain.workspace,domain.owner,event.event_id],|row|row.get(0))?;
            let record_sha256:[u8;32]=stored.try_into().map_err(|_|anyhow::anyhow!("Router committed receipt invalid"))?;
            anyhow::ensure!(record_sha256==fingerprint,"Router committed receipt conflicts");
            check(deadline)?;tx.commit()?;
            Ok(RouterHistoryCommitReceipt {terminal_event_id:event.event_id.clone(),record_sha256,replayed:false})
        }).await?
    }
}

fn validate_terminal(tx: &Transaction<'_>, evidence: &RouterLearningEvidence) -> anyhow::Result<i64> {
    let event = &evidence.terminal;
    let sequence: Option<i64> = tx.query_row("SELECT s.id FROM message_events t JOIN message_events s
                ON s.event_id=?1 WHERE t.event_id=?2 AND t.workspace_id=?3 AND t.owner_id=?4
                AND t.session_key=?5 AND t.run_id=?6 AND t.event_type='turn.finalized' AND t.role='event'
                AND t.idempotency_key=?7 AND t.raw_payload_json=?8 AND t.content=?9
                AND t.source=?10 AND t.channel IS ?11 AND t.sender IS ?12
                AND s.workspace_id=t.workspace_id AND s.owner_id=t.owner_id AND s.session_key=t.session_key
                AND s.run_id=t.run_id AND s.source=t.source AND s.channel IS t.channel
                AND s.sender IS t.sender AND s.role='user' AND s.event_type='message.created'
                AND s.idempotency_key=('agent_api:' || t.run_id || ':input')
                AND s.visibility='session' AND s.id<t.id AND s.config_generation_id IS t.config_generation_id
                AND s.config_source_revision IS t.config_source_revision AND t.id=?13 AND t.content_hash IS ?14
                    AND t.visibility='session' AND t.recipient=t.session_key AND t.causation_event_id=?1
                    AND t.parent_session_key IS NULL AND t.parent_run_id IS NULL
                    AND t.agent_id IS NULL AND t.persona_id IS NULL AND t.subject_ref_json IS NULL
                    AND t.goal_id IS NULL AND t.correlation_id IS NULL AND t.attempt_id IS NULL AND t.lease_epoch IS NULL
                    AND t.config_generation_id=?15 AND t.config_source_revision=?16
                    AND t.source_ref_json=?17
                    AND s.parent_session_key IS NULL AND s.parent_run_id IS NULL
                    AND s.agent_id IS NULL AND s.persona_id IS NULL AND s.subject_ref_json IS NULL
                    AND s.goal_id IS NULL AND s.correlation_id IS NULL AND s.attempt_id IS NULL AND s.lease_epoch IS NULL
                    AND s.causation_event_id IS NULL AND s.recipient=t.recipient AND s.raw_payload_json IS ?20
                    AND s.content=?18 AND s.content_hash=?19 AND s.source_ref_json=?17
",
                params![evidence.source_event_id,event.event_id,event.workspace_id,event.owner_id,event.session_key,
                    evidence.terminal_id(),event.idempotency_key,event.raw_payload_json,event.content,event.source.as_str(),event.channel,event.sender,
                    event.id,event.content_hash,evidence.generation,evidence.revision,
                    serde_json::to_string(&event.source)?,evidence.source_input,SqliteMemory::content_hash(&evidence.source_input),evidence.source_raw_payload],
                |row| row.get(0)).optional()?;
    let sequence = sequence.context("Router history source or terminal authority mismatch")?;
    anyhow::ensure!(
        sequence > 0 && sequence < event.id,
        "Router history source sequence invalid"
    );
    Ok(sequence)
}
