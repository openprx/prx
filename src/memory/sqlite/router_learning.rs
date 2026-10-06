//! Bounded runtime statistics, separate from retrieval authorization.
use super::{Arc, Connection, Context, OptionalExtension, SqliteMemory, params};
use crate::memory::traits::{RouterLearningEvidence, RouterLearningRead, RouterLearningSnapshot};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl SqliteMemory {
    pub(super) async fn router_learning_transaction(
        &self,
        evidence: &RouterLearningEvidence,
        update: Option<(i64, RouterLearningSnapshot)>,
        deadline: Instant,
    ) -> anyhow::Result<(RouterLearningRead, bool)> {
        evidence.validate()?;
        let fingerprint = evidence.fingerprint()?;
        let domain = evidence.domain();
        let encoded = update.as_ref().map(|(_, next)| next.encode()).transpose()?;
        let evidence = evidence.clone();
        let path = self.db_path.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let _owner = CancelOnDrop(Arc::clone(&cancelled));
        crate::runtime::blocking::spawn_blocking(move || -> anyhow::Result<_> {
            let check = || -> anyhow::Result<()> {
                anyhow::ensure!(Instant::now() < deadline && !cancelled.load(Ordering::Acquire),
                    "Router learning deadline expired or cancelled"); Ok(())
            };
            check()?;
            let parent = path.parent().context("Router database parent missing")?.canonicalize()?;
            let path = parent.join(path.file_name().context("Router database filename missing")?);
            let mut conn = Connection::open_with_flags(path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
            conn.busy_timeout(std::time::Duration::from_millis(25))?;
            let stop = Arc::clone(&cancelled);
            conn.progress_handler(1000, Some(move || Instant::now() >= deadline || stop.load(Ordering::Acquire)))?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let event = &evidence.terminal;
            let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM message_events t JOIN message_events s
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
)",
                params![evidence.source_event_id,event.event_id,event.workspace_id,event.owner_id,event.session_key,
                    evidence.terminal_id(),event.idempotency_key,event.raw_payload_json,event.content,event.source.as_str(),event.channel,event.sender,
                    event.id,event.content_hash,evidence.generation,evidence.revision,
                    serde_json::to_string(&event.source)?,evidence.source_input,Self::content_hash(&evidence.source_input),evidence.source_raw_payload],
                |row| row.get(0))?;
            anyhow::ensure!(valid, "Router learning source or terminal authority mismatch");
            // New tables only: no historical/global rows are scanned or imported.
            tx.execute_batch("CREATE TABLE IF NOT EXISTS router_learning_state (
                domain TEXT PRIMARY KEY, version INTEGER NOT NULL, snapshot TEXT NOT NULL CHECK(length(CAST(snapshot AS BLOB)) <= 262144));
                CREATE TABLE IF NOT EXISTS router_learning_receipts (
                workspace TEXT NOT NULL, owner TEXT NOT NULL, terminal TEXT NOT NULL,
                domain TEXT NOT NULL, fingerprint TEXT NOT NULL,
                PRIMARY KEY(workspace,owner,terminal));")?;
            let replay: Option<(String,String)> = tx.query_row(
                "SELECT CASE WHEN length(domain)=64 THEN domain ELSE '' END,CASE WHEN length(fingerprint)=64 THEN fingerprint ELSE '' END FROM router_learning_receipts WHERE workspace=?1 AND owner=?2 AND terminal=?3",
                params![event.workspace_id,event.owner_id,evidence.terminal_id()], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
            if let Some((stored_domain, stored_fingerprint)) = &replay {
                anyhow::ensure!(stored_domain == &domain && stored_fingerprint == &fingerprint,
                    "Router learning terminal replay conflicts");
            }
            // CASE bounds data before row materialization; oversized/corrupt state is an error, never absent.
            let stored: Option<(i64, Option<String>)> = tx.query_row(
                "SELECT version,CASE WHEN length(CAST(snapshot AS BLOB)) <= 262144 THEN snapshot END FROM router_learning_state WHERE domain=?1",
                [&domain], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
            let snapshot = stored.map(|(version,value)| -> anyhow::Result<_> {
                let value = value.context("Router snapshot exceeds byte bound")?;
                let snapshot = RouterLearningSnapshot::decode(&value)?;
                anyhow::ensure!(snapshot.version == version, "Router snapshot version mismatch"); Ok(snapshot)
            }).transpose()?;
            if replay.is_some() {
                anyhow::ensure!(snapshot.is_some(), "Router replay snapshot missing");
                check()?; tx.commit()?;
                return Ok((RouterLearningRead {snapshot,replayed:true}, false));
            }
            if let Some((expected,next)) = update {
                let version = snapshot.as_ref().map_or(0, |state| state.version);
                if version != expected { return Ok((RouterLearningRead {snapshot,replayed:false}, true)); }
                anyhow::ensure!(expected.checked_add(1)==Some(next.version), "Router next version invalid");
                check()?;
                tx.execute("INSERT INTO router_learning_state(domain,version,snapshot) VALUES(?1,?2,?3)
                    ON CONFLICT(domain) DO UPDATE SET version=excluded.version,snapshot=excluded.snapshot",
                    params![domain,next.version,encoded.context("Router update missing")?])?;
                tx.execute("INSERT INTO router_learning_receipts(workspace,owner,terminal,domain,fingerprint) VALUES(?1,?2,?3,?4,?5)",
                    params![event.workspace_id,event.owner_id,evidence.terminal_id(),domain,fingerprint])?;
                check()?; tx.commit()?;
                Ok((RouterLearningRead {snapshot:Some(next),replayed:false}, false))
            } else {
                check()?; tx.commit()?;
                Ok((RouterLearningRead {snapshot,replayed:false}, false))
            }
        }).await?
    }
}
