//! Owner-scoped Router history on an owned, deadline-bound PG connection.
use super::PostgresMemory;
use crate::memory::router_history::{
    ROUTER_HISTORY_MAX_RECORDS, RouterHistoryCommitReceipt, RouterHistoryDomain, RouterHistoryRead,
    RouterHistoryReadEvidence, RouterHistoryRow, RouterHistoryWriteEvidence, decode_vector,
};
use crate::memory::traits::RouterLearningEvidence;
use anyhow::Context;
use sha2::{Digest, Sha256};
use std::time::Instant;
use tokio_postgres::Transaction;

fn check(deadline: Instant) -> anyhow::Result<()> {
    anyhow::ensure!(Instant::now() < deadline, "Router history deadline expired");
    Ok(())
}
fn configuration(memory: &PostgresMemory, deadline: Instant) -> anyhow::Result<tokio_postgres::Config> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .context("Router history deadline expired")?;
    let mut config: tokio_postgres::Config = memory
        .client
        .db_url
        .parse()
        .context("Router PostgreSQL configuration invalid")?;
    config.connect_timeout(remaining);
    let ms = remaining.as_millis().max(1).min(i32::MAX as u128);
    config.options(&format!(
        "{} -c statement_timeout={ms} -c lock_timeout={ms}",
        config.get_options().unwrap_or_default()
    ));
    Ok(config)
}
async fn budget(tx: &Transaction<'_>, deadline: Instant) -> anyhow::Result<()> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .context("Router history deadline expired")?;
    let ms = remaining.as_millis().max(1).min(i32::MAX as u128).to_string();
    tx.query_one(
        "SELECT set_config('statement_timeout',$1,true),set_config('lock_timeout',$1,true)",
        &[&ms],
    )
    .await?;
    check(deadline)
}
async fn scope(tx: &Transaction<'_>, domain: &RouterHistoryDomain) -> anyhow::Result<()> {
    tx.query_one(
        "SELECT set_config('prx.rls_bypass','off',true),set_config('prx.current_workspace',$1,true),
        set_config('prx.current_owner',$2,true),set_config('prx.owner_isolated','on',true)",
        &[&domain.workspace, &domain.owner],
    )
    .await?;
    Ok(())
}
/// Full 256-bit digest encoded as identifier-safe base32 (52 bytes). Hash
/// the real qualified message-event relation, never a caller workspace/name().
pub(super) fn namespace_digest(qualified_message_events_table: &str) -> anyhow::Result<String> {
    let digest = Sha256::digest(qualified_message_events_table.as_bytes());
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut encoded = String::with_capacity(52);
    let mut pending = 0u16;
    let mut bits = 0u8;
    for byte in digest {
        pending = (pending << 8) | u16::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            encoded.push(char::from(
                *ALPHABET
                    .get(usize::from((pending >> bits) & 31))
                    .context("invalid base32 digit")?,
            ));
        }
        pending &= (1u16 << bits) - 1;
    }
    if bits > 0 {
        encoded.push(char::from(
            *ALPHABET
                .get(usize::from((pending << (5 - bits)) & 31))
                .context("invalid base32 final digit")?,
        ));
    }
    Ok(encoded)
}

fn namespace(memory: &PostgresMemory) -> anyhow::Result<(String, String)> {
    let encoded = namespace_digest(&memory.qualified_message_events_table)?;
    let table = format!("rh_v1_{encoded}");
    let index = format!("rhi_v1_{encoded}");
    super::validate_identifier(&table, "Router history table")?;
    super::validate_identifier(&index, "Router history index")?;
    Ok((
        format!("{}.{}", memory.schema_ident, super::quote_identifier(&table)),
        super::quote_identifier(&index),
    ))
}
async fn schema(tx: &Transaction<'_>, table: &str, index: &str, deadline: Instant) -> anyhow::Result<()> {
    budget(tx, deadline).await?;
    let missing: bool = tx.query_one("SELECT to_regclass($1) IS NULL", &[&table]).await?.get(0);
    if missing {
        let key = format!("{table}:router-history-schema-v1");
        budget(tx, deadline).await?;
        tx.query_one("SELECT pg_advisory_xact_lock(hashtextextended($1,0))", &[&key])
            .await?;
        budget(tx, deadline).await?;
        tx.batch_execute(&format!("CREATE TABLE IF NOT EXISTS {table} (
            domain TEXT NOT NULL,workspace TEXT NOT NULL,owner TEXT NOT NULL,session TEXT NOT NULL,
            policy BYTEA NOT NULL CHECK(octet_length(policy)=32),embedding_model TEXT NOT NULL,
            dimensions INTEGER NOT NULL CHECK(dimensions BETWEEN 1 AND 2048),embedding_version INTEGER NOT NULL CHECK(embedding_version=1),
            source_event TEXT NOT NULL,source_sequence BIGINT NOT NULL,terminal TEXT NOT NULL,terminal_sequence BIGINT NOT NULL,
            provider TEXT NOT NULL,model TEXT NOT NULL,success BOOLEAN NOT NULL,
            embedding BYTEA NOT NULL CHECK(octet_length(embedding)=dimensions*4),
            fingerprint BYTEA NOT NULL CHECK(octet_length(fingerprint)=32),
            PRIMARY KEY(workspace,owner,terminal),CHECK(source_sequence>0 AND terminal_sequence>source_sequence));
            CREATE INDEX IF NOT EXISTS {index} ON {table}(domain,source_sequence,terminal_sequence);")).await?;
    }
    Ok(())
}
async fn source_sequence(
    tx: &Transaction<'_>,
    table: &str,
    evidence: &RouterHistoryReadEvidence,
) -> anyhow::Result<i64> {
    let source = &evidence.source;
    let domain = &evidence.domain;
    let row = tx
        .query_opt(
            &format!(
                "SELECT id FROM {table} WHERE event_id=$1
        AND workspace_id=$2 AND owner_id=$3 AND session_key=$4 AND run_id=$5
        AND source='agent' AND source_ref_json=$6 AND channel='agent_api' AND sender='embedded-caller'
        AND role='user' AND event_type='message.created' AND idempotency_key=('agent_api:' || $5::TEXT || ':input')
        AND content=$7 AND content_hash=$8 AND raw_payload_json IS NOT DISTINCT FROM $9::TEXT
        AND config_generation_id=$10 AND config_source_revision=$11
        AND visibility='session' AND recipient=session_key
        AND parent_session_key IS NULL AND parent_run_id IS NULL AND agent_id IS NULL AND persona_id IS NULL
        AND subject_ref_json IS NULL AND goal_id IS NULL AND causation_event_id IS NULL AND correlation_id IS NULL
        AND attempt_id IS NULL AND lease_epoch IS NULL FOR SHARE"
            ),
            &[
                &source.event_id,
                &domain.workspace,
                &domain.owner,
                &domain.session,
                &source.run,
                &serde_json::to_string(&crate::memory::MessageEventSource::Agent)?,
                &source.input,
                &PostgresMemory::content_hash(&source.input),
                &source.raw_payload,
                &source.generation,
                &source.revision,
            ],
        )
        .await?;
    let id: i64 = row.context("Router history current source authority mismatch")?.get(0);
    anyhow::ensure!(id > 0, "Router history source sequence invalid");
    Ok(id)
}

const COLUMNS: &str = "source_sequence,terminal_sequence,
    CASE WHEN octet_length(terminal) BETWEEN 1 AND 512 THEN terminal END,
    CASE WHEN octet_length(provider) BETWEEN 1 AND 512 THEN provider END,
    CASE WHEN octet_length(model) BETWEEN 1 AND 512 THEN model END,success,
    CASE WHEN octet_length(embedding)<=8192 THEN embedding END,
    CASE WHEN octet_length(workspace) BETWEEN 1 AND 512 THEN workspace END,
    CASE WHEN octet_length(owner) BETWEEN 1 AND 512 THEN owner END,
    CASE WHEN octet_length(session) BETWEEN 1 AND 512 THEN session END,
    CASE WHEN octet_length(policy)=32 THEN policy END,
    CASE WHEN octet_length(embedding_model) BETWEEN 1 AND 512 THEN embedding_model END,
    dimensions,embedding_version,
    CASE WHEN octet_length(fingerprint)=32 THEN fingerprint END,
    CASE WHEN octet_length(source_event) BETWEEN 1 AND 512 THEN source_event END";
fn decode(
    row: &tokio_postgres::Row,
    domain: &RouterHistoryDomain,
) -> anyhow::Result<(RouterHistoryRow, [u8; 32], String)> {
    let string = |index| -> anyhow::Result<String> {
        row.try_get::<_, Option<String>>(index)?
            .context("Router history metadata exceeds bounds")
    };
    let bytes = |index| -> anyhow::Result<Vec<u8>> {
        row.try_get::<_, Option<Vec<u8>>>(index)?
            .context("Router history binary value exceeds bounds")
    };
    anyhow::ensure!(
        string(7)? == domain.workspace
            && string(8)? == domain.owner
            && string(9)? == domain.session
            && bytes(10)?.as_slice() == domain.policy_sha256
            && string(11)? == domain.embedding_model
            && row.try_get::<_, i32>(12)? == i32::from(domain.dimensions)
            && row.try_get::<_, i32>(13)? == i32::from(domain.embedding_version),
        "Router history stored domain conflicts"
    );
    let fingerprint = bytes(14)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid Router history receipt"))?;
    let result = RouterHistoryRow {
        source_sequence: row.try_get(0)?,
        terminal_sequence: row.try_get(1)?,
        terminal_event_id: string(2)?,
        provider: string(3)?,
        model: string(4)?,
        success: row.try_get(5)?,
        embedding: decode_vector(&bytes(6)?, domain.dimensions)?,
    };
    Ok((result, fingerprint, string(15)?))
}

impl PostgresMemory {
    pub(super) async fn router_history_read(
        &self,
        evidence: RouterHistoryReadEvidence,
        deadline: Instant,
    ) -> anyhow::Result<RouterHistoryRead> {
        evidence.validate(deadline)?;
        let config = configuration(self, deadline)?;
        let operation = async {
            let (mut client, connection) = config.connect(tokio_postgres::NoTls).await?;
            tokio::pin!(connection);
            let transaction = async {
                let tx = client
                    .build_transaction()
                    .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
                    .start()
                    .await?;
                budget(&tx, deadline).await?;
                scope(&tx, &evidence.domain).await?;
                budget(&tx, deadline).await?;
                let sequence = source_sequence(&tx, &self.qualified_message_events_table, &evidence).await?;
                let (table, _index) = namespace(self)?;
                budget(&tx, deadline).await?;
                let exists: bool = tx
                    .query_one("SELECT to_regclass($1) IS NOT NULL", &[&table])
                    .await?
                    .get(0);
                let mut result = RouterHistoryRead {
                    source_sequence: sequence,
                    rows: Vec::new(),
                };
                if exists {
                    let key = evidence.domain.key()?;
                    budget(&tx, deadline).await?;
                    let bounds=tx.query_one(&format!("SELECT count(*),coalesce(bool_and(
                        octet_length(terminal) BETWEEN 1 AND 512 AND octet_length(provider) BETWEEN 1 AND 512
                        AND octet_length(model) BETWEEN 1 AND 512 AND octet_length(workspace) BETWEEN 1 AND 512
                        AND octet_length(owner) BETWEEN 1 AND 512 AND octet_length(session) BETWEEN 1 AND 512
                        AND octet_length(embedding_model) BETWEEN 1 AND 512 AND octet_length(source_event) BETWEEN 1 AND 512
                        AND octet_length(policy)=32 AND octet_length(fingerprint)=32
                        AND dimensions BETWEEN 1 AND 2048 AND embedding_version=1 AND octet_length(embedding)=dimensions*4),true)
                        FROM (SELECT * FROM {table} WHERE domain=$1 AND source_sequence<$2 AND terminal_sequence<$2 LIMIT $3) bounded"),
                        &[&key,&sequence,&((ROUTER_HISTORY_MAX_RECORDS+1) as i64)]).await?;
                    anyhow::ensure!(
                        bounds.try_get::<_, i64>(0)? <= ROUTER_HISTORY_MAX_RECORDS as i64,
                        "Router history complete domain exceeds capacity"
                    );
                    anyhow::ensure!(
                        bounds.try_get::<_, bool>(1)?,
                        "Router history row exceeds encoding bounds"
                    );
                    check(deadline)?;
                    budget(&tx, deadline).await?;
                    let rows = tx
                        .query(
                            &format!(
                                "SELECT {COLUMNS} FROM {table}
                        WHERE domain=$1 AND source_sequence<$2 AND terminal_sequence<$2
                        ORDER BY source_sequence,terminal_sequence LIMIT $3"
                            ),
                            &[&key, &sequence, &((ROUTER_HISTORY_MAX_RECORDS + 1) as i64)],
                        )
                        .await?;
                    anyhow::ensure!(
                        rows.len() <= ROUTER_HISTORY_MAX_RECORDS,
                        "Router history complete domain exceeds capacity"
                    );
                    for row in rows {
                        check(deadline)?;
                        result.rows.push(decode(&row, &evidence.domain)?.0);
                    }
                }
                result.validate(&evidence.domain)?;
                budget(&tx, deadline).await?;
                tx.commit().await?;
                Ok(result)
            };
            // The caller owns both futures. Timeout/drop closes the actual
            // protocol connection; there is no detached driver or pool waiter.
            tokio::select! {result=transaction=>result,result=&mut connection=>{result?;anyhow::bail!("Router PostgreSQL protocol closed")}}
        };
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), operation)
            .await
            .context("Router history deadline expired")?
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
        let domain = &evidence.domain;
        let key = domain.key()?;
        let evidence = &evidence.terminal;
        let event = &evidence.terminal;
        let config = configuration(self, deadline)?;
        let operation = async {
            let (mut client, connection) = config.connect(tokio_postgres::NoTls).await?;
            tokio::pin!(connection);
            let transaction = async {
                let tx: tokio_postgres::Transaction<'_> = client.transaction().await?;
                budget(&tx, deadline).await?;
                scope(&tx, domain).await?;
                budget(&tx, deadline).await?;
                let sequence = validate_terminal(&tx, &self.qualified_message_events_table, evidence).await?;
                let (table, index) = namespace(self)?;
                schema(&tx, &table, &index, deadline).await?;
                // Terminal lock crosses policy domains. Domain lock serializes
                // capacity admission including the empty-table/absent-row case.
                for lock in [
                    format!(
                        "{table}:router-history-terminal:{}:{}:{}",
                        domain.workspace, domain.owner, event.event_id
                    ),
                    format!("{table}:router-history-domain:{key}"),
                ] {
                    budget(&tx, deadline).await?;
                    tx.query_one("SELECT pg_advisory_xact_lock(hashtextextended($1,0))", &[&lock])
                        .await?;
                }
                budget(&tx, deadline).await?;
                let replay = tx
                    .query_opt(
                        &format!(
                            "SELECT {COLUMNS},CASE WHEN octet_length(domain)=64 THEN domain END FROM {table}
                    WHERE workspace=$1 AND owner=$2 AND terminal=$3"
                        ),
                        &[&domain.workspace, &domain.owner, &event.event_id],
                    )
                    .await?;
                if let Some(row) = replay {
                    let (stored, digest, source) = decode(&row, domain)?;
                    anyhow::ensure!(
                        row.try_get::<_, Option<String>>(16)?.as_deref() == Some(key.as_str())
                            && digest == fingerprint
                            && source == evidence.source_event_id
                            && stored.source_sequence == sequence
                            && stored.terminal_sequence == event.id
                            && stored.provider == evidence.provider
                            && stored.model == evidence.model
                            && stored.success == evidence.success
                            && stored
                                .embedding
                                .iter()
                                .flat_map(|value| value.to_le_bytes())
                                .eq(vector.iter().copied()),
                        "Router history terminal replay conflicts"
                    );
                    budget(&tx, deadline).await?;
                    tx.commit().await?;
                    return Ok(RouterHistoryCommitReceipt {
                        terminal_event_id: event.event_id.clone(),
                        record_sha256: digest,
                        replayed: true,
                    });
                }
                budget(&tx, deadline).await?;
                let count: i64 = tx
                    .query_one(
                        &format!("SELECT count(*) FROM (SELECT 1 FROM {table} WHERE domain=$1 LIMIT $2) bounded"),
                        &[&key, &((ROUTER_HISTORY_MAX_RECORDS + 1) as i64)],
                    )
                    .await?
                    .get(0);
                anyhow::ensure!(
                    count < (ROUTER_HISTORY_MAX_RECORDS as i64),
                    "Router history complete domain capacity exhausted; receipt ledger retained"
                );
                budget(&tx, deadline).await?;
                tx.execute(&format!("INSERT INTO {table}(domain,workspace,owner,session,policy,embedding_model,dimensions,embedding_version,
                    source_event,source_sequence,terminal,terminal_sequence,provider,model,success,embedding,fingerprint)
                    VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)"),
                    &[&key,&domain.workspace,&domain.owner,&domain.session,&&domain.policy_sha256[..],&domain.embedding_model,
                        &i32::from(domain.dimensions),&i32::from(domain.embedding_version),&evidence.source_event_id,&sequence,
                        &event.event_id,&event.id,&evidence.provider,&evidence.model,&evidence.success,&vector,&&fingerprint[..]]).await?;
                budget(&tx, deadline).await?;
                let row = tx
                    .query_one(
                        &format!(
                            "SELECT CASE WHEN octet_length(fingerprint)=32 THEN fingerprint END FROM {table}
                    WHERE workspace=$1 AND owner=$2 AND terminal=$3"
                        ),
                        &[&domain.workspace, &domain.owner, &event.event_id],
                    )
                    .await?;
                let stored: Vec<u8> = row.try_get(0)?;
                let record_sha256: [u8; 32] = stored
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("Router committed receipt invalid"))?;
                anyhow::ensure!(record_sha256 == fingerprint, "Router committed receipt conflicts");
                budget(&tx, deadline).await?;
                tx.commit().await?;
                Ok(RouterHistoryCommitReceipt {
                    terminal_event_id: event.event_id.clone(),
                    record_sha256,
                    replayed: false,
                })
            };
            tokio::select! {result=transaction=>result,result=&mut connection=>{result?;anyhow::bail!("Router PostgreSQL protocol closed")}}
        };
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), operation)
            .await
            .context("Router history deadline expired")?
    }
}

async fn validate_terminal(
    tx: &Transaction<'_>,
    events: &str,
    evidence: &RouterLearningEvidence,
) -> anyhow::Result<i64> {
    let event = &evidence.terminal;
    let sql = format!("SELECT s.id FROM {events} t JOIN {events} s ON s.event_id=$1
                    WHERE t.event_id=$2 AND t.workspace_id=$3 AND t.owner_id=$4 AND t.session_key=$5 AND t.run_id=$6
                    AND t.event_type='turn.finalized' AND t.role='event' AND t.idempotency_key=$7
                    AND t.raw_payload_json=$8 AND t.content=$9 AND t.source=$10
                    AND t.channel IS NOT DISTINCT FROM $11::TEXT AND t.sender IS NOT DISTINCT FROM $12::TEXT
                    AND s.workspace_id=t.workspace_id AND s.owner_id=t.owner_id AND s.session_key=t.session_key
                    AND s.run_id=t.run_id AND s.source=t.source AND s.channel IS NOT DISTINCT FROM t.channel
                    AND s.sender IS NOT DISTINCT FROM t.sender AND s.role='user' AND s.event_type='message.created'
                    AND s.idempotency_key=('agent_api:' || t.run_id || ':input')
                AND s.visibility='session' AND s.id<t.id AND s.config_generation_id IS NOT DISTINCT FROM t.config_generation_id
                    AND s.config_source_revision IS NOT DISTINCT FROM t.config_source_revision AND t.id=$13 AND t.content_hash IS NOT DISTINCT FROM $14
                    AND t.visibility='session' AND t.recipient=t.session_key AND t.causation_event_id=$1
                    AND t.parent_session_key IS NULL AND t.parent_run_id IS NULL
                    AND t.agent_id IS NULL AND t.persona_id IS NULL AND t.subject_ref_json IS NULL
                    AND t.goal_id IS NULL AND t.correlation_id IS NULL AND t.attempt_id IS NULL AND t.lease_epoch IS NULL
                    AND t.config_generation_id=$15 AND t.config_source_revision=$16
                    AND t.source_ref_json=$17
                    AND s.parent_session_key IS NULL AND s.parent_run_id IS NULL
                    AND s.agent_id IS NULL AND s.persona_id IS NULL AND s.subject_ref_json IS NULL
                    AND s.goal_id IS NULL AND s.correlation_id IS NULL AND s.attempt_id IS NULL AND s.lease_epoch IS NULL
                    AND s.causation_event_id IS NULL AND s.recipient=t.recipient AND s.raw_payload_json IS NOT DISTINCT FROM $20::TEXT
                    AND s.content=$18 AND s.content_hash=$19 AND s.source_ref_json=$17
 FOR SHARE OF t,s", events=events);
    let row = tx
        .query_opt(
            &sql,
            &[
                &evidence.source_event_id,
                &event.event_id,
                &event.workspace_id,
                &event.owner_id,
                &event.session_key,
                &evidence.terminal_id(),
                &event.idempotency_key,
                &event.raw_payload_json,
                &event.content,
                &event.source.as_str(),
                &event.channel,
                &event.sender,
                &event.id,
                &event.content_hash,
                &evidence.generation,
                &evidence.revision,
                &serde_json::to_string(&event.source)?,
                &evidence.source_input,
                &PostgresMemory::content_hash(&evidence.source_input),
                &evidence.source_raw_payload,
            ],
        )
        .await?;
    let sequence: i64 = row
        .context("Router history source or terminal authority mismatch")?
        .get(0);
    anyhow::ensure!(
        sequence > 0 && sequence < event.id,
        "Router history source sequence invalid"
    );
    Ok(sequence)
}
