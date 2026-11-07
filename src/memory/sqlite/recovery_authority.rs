//! Lazy recovery authority, owned by the SQLite writer transaction.
use super::{SqliteMemory, recovery_keys::KeyDirectory};
use crate::memory::ingress_recovery::{
    IngressAttestation, IngressAttestationError, IngressCapsule, IngressRecoveryScope, PendingFacts,
};
use crate::memory::recovery_authority::{
    RecoveryChain, RecoveryEvidence, RecoveryFactsTooLarge, RecoveryKeyUnavailable, RecoveryMacDomain,
    RecoveryPublication, RecoveryReceipt, RecoveryRepository, RecoveryTag,
};
use crate::memory::traits::MessageEvent;
use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, types::ValueRef};
use serde::{Deserialize, Serialize};

/// Number of stripes in the fixed per-key gate set.
pub(in crate::memory::sqlite) const RECOVERY_KEY_GATES: usize = 16;

/// Stripe for a key id. Equal ids always share a stripe, so per-id serialization
/// holds; distinct ids may share one, which only adds harmless waiting.
fn recovery_key_gate_index(id: &str) -> usize {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    // The remainder is below RECOVERY_KEY_GATES, so the cast cannot truncate.
    (hasher.finish() % RECOVERY_KEY_GATES as u64) as usize
}

const MAX_SERIALIZED_FACTS: usize = 262_144;
// Six identity/event-id strings of at most 1024 bytes expand by at most 6x.
// 40 KiB covers those strings, two 32-byte JSON digests, and structural keys.
// Each byte field expands by at most 4x; reserve equal pending/completion budgets.
const MAX_FACTS: usize = (MAX_SERIALIZED_FACTS - 40 * 1024) / 8;
const MAX_RECORD: usize = 1_048_576;

/// Key material could not be loaded or trusted. Only this class is memoized
/// per handle; schema and database failures stay retryable.
fn key_unavailable(error: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(RecoveryKeyUnavailable(format!("{error:#}")))
}

pub(super) fn schema(conn: &Connection) -> anyhow::Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='recovery_authority' AND type='table')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        conn.execute_batch(&format!(
            "CREATE TABLE recovery_authority (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            version INTEGER NOT NULL CHECK(version=1), backend_namespace TEXT NOT NULL,
            active_key_id TEXT NOT NULL
        );
        CREATE TABLE recovery_attestations (
            workspace_id TEXT NOT NULL, owner_id TEXT NOT NULL, source TEXT NOT NULL, run_id TEXT NOT NULL,
            input_event_id TEXT REFERENCES message_events(event_id) ON DELETE CASCADE,
            terminal_event_id TEXT REFERENCES message_events(event_id) ON DELETE CASCADE,
            version INTEGER NOT NULL CHECK(version=1), key_id TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('pending','settled')),
            facts BLOB NOT NULL CHECK(length(facts)<={MAX_SERIALIZED_FACTS}), mac BLOB NOT NULL CHECK(length(mac)=32),
            PRIMARY KEY(workspace_id,owner_id,source,run_id),
            CHECK((state='pending' AND input_event_id IS NOT NULL AND terminal_event_id IS NULL)
                  OR (state='settled' AND terminal_event_id IS NOT NULL)),
            UNIQUE(workspace_id,owner_id,source,input_event_id),
            UNIQUE(workspace_id,owner_id,source,terminal_event_id)
        );
        CREATE TRIGGER recovery_settled_immutable BEFORE UPDATE ON recovery_attestations
        WHEN OLD.state='settled' OR NEW.state<>'settled'
             OR OLD.workspace_id IS NOT NEW.workspace_id OR OLD.owner_id IS NOT NEW.owner_id
             OR OLD.source IS NOT NEW.source OR OLD.run_id IS NOT NEW.run_id
             OR OLD.input_event_id IS NOT NEW.input_event_id
        BEGIN SELECT RAISE(ABORT,'immutable recovery receipt'); END;"
        ))?;
    }
    super::erasure_pool::install_table_triggers(conn, "recovery_attestations")?;
    transport_schema(conn)
}

/// AUTH-02-1b tables, added lazily in the writer transaction so a database
/// that already holds AUTH-01 state migrates in place. The tombstone table has
/// no foreign key and is not erasure-indexed: it must outlive the erased input,
/// and it holds only the keyed digest of the stable key, never content.
fn transport_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS recovery_transport_index (
            workspace_id TEXT NOT NULL, owner_id TEXT NOT NULL, source TEXT NOT NULL, run_id TEXT NOT NULL,
            transport_digest BLOB NOT NULL CHECK(length(transport_digest)=32),
            input_key_digest BLOB NOT NULL CHECK(length(input_key_digest)=32),
            key_id TEXT NOT NULL,
            PRIMARY KEY(workspace_id,owner_id,source,run_id),
            FOREIGN KEY(workspace_id,owner_id,source,run_id) REFERENCES recovery_attestations ON DELETE CASCADE
        );
        CREATE TABLE IF NOT EXISTS recovery_tombstones (
            workspace_id TEXT NOT NULL, owner_id TEXT NOT NULL, source TEXT NOT NULL,
            input_key_digest BLOB NOT NULL CHECK(length(input_key_digest)=32),
            key_id TEXT NOT NULL,
            UNIQUE(workspace_id,owner_id,source,input_key_digest)
        );
        CREATE TRIGGER IF NOT EXISTS recovery_transport_tombstone AFTER DELETE ON recovery_transport_index
        BEGIN
            INSERT OR IGNORE INTO recovery_tombstones(workspace_id,owner_id,source,input_key_digest,key_id)
            VALUES(OLD.workspace_id,OLD.owner_id,OLD.source,OLD.input_key_digest,OLD.key_id);
        END;",
    )?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
struct BoundEvent {
    id: String,
    digest: Vec<u8>,
}
#[derive(Clone, Serialize, Deserialize)]
struct BoundFacts {
    chain: RecoveryChain,
    input: Option<BoundEvent>,
    terminal: Option<BoundEvent>,
    pending: Vec<u8>,
    completion: Vec<u8>,
}

fn validate_chain(chain: &RecoveryChain) -> anyhow::Result<()> {
    for field in [&chain.workspace_id, &chain.owner_id, &chain.source, &chain.run_id] {
        anyhow::ensure!(!field.is_empty() && field.len() <= 1024, "invalid recovery identity");
    }
    Ok(())
}

/// Encode every persisted column (including raw source/capsule bytes), with
/// explicit type/length framing. No lossy MessageEvent normalization is used.
fn event(conn: &Connection, chain: &RecoveryChain, id: &str) -> anyhow::Result<RecoveryEvidence> {
    validate_chain(chain)?;
    anyhow::ensure!(!id.is_empty() && id.len() <= 1024, "invalid recovery event id");
    let record = conn.query_row("SELECT * FROM message_events WHERE event_id=?1 AND workspace_id=?2 AND owner_id=?3 AND source=?4 AND run_id=?5", params![id,chain.workspace_id,chain.owner_id,chain.source,chain.run_id], |row| {
        let mut record = Vec::new();
        for index in 0..row.as_ref().column_count() {
            let value = row.get_ref(index)?;
            let (tag, bytes) = match value {
                ValueRef::Null => (0, Vec::new()),
                ValueRef::Integer(value) => (1, value.to_be_bytes().to_vec()),
                ValueRef::Real(value) => (2, value.to_bits().to_be_bytes().to_vec()),
                ValueRef::Text(value) | ValueRef::Blob(value) => {
                    if value.len() > MAX_RECORD { return Err(rusqlite::Error::InvalidQuery); }
                    (if matches!(row.get_ref(index)?, ValueRef::Text(_)) { 3 } else { 4 }, value.to_vec())
                }
            };
            if record.len().saturating_add(bytes.len()).saturating_add(9) > MAX_RECORD {
                return Err(rusqlite::Error::InvalidQuery);
            }
            record.push(tag);
            record.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            record.extend_from_slice(&bytes);
        }
        Ok(record)
    })?;
    Ok(RecoveryEvidence {
        event_id: id.to_owned(),
        record,
    })
}

fn framed(domain: &[u8], bytes: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(16 + domain.len() + bytes.len());
    framed.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    framed.extend_from_slice(domain);
    framed.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    framed.extend_from_slice(bytes);
    framed
}
fn domain_mac(key: &ring::hmac::Key, domain: &[u8], bytes: &[u8]) -> Vec<u8> {
    ring::hmac::sign(key, &framed(domain, bytes)).as_ref().to_vec()
}
fn bind(
    conn: &Connection,
    chain: &RecoveryChain,
    evidence: &RecoveryEvidence,
    key: &ring::hmac::Key,
) -> anyhow::Result<BoundEvent> {
    anyhow::ensure!(
        evidence.record.len() <= MAX_RECORD && event(conn, chain, &evidence.event_id)? == *evidence,
        "recovery event changed"
    );
    Ok(BoundEvent {
        id: evidence.event_id.clone(),
        digest: domain_mac(key, b"prx.recovery.event.v1", &evidence.record),
    })
}
fn receipt_bytes(receipt: &RecoveryReceipt) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(&(
        receipt.version,
        &receipt.backend_namespace,
        &receipt.key_id,
        &receipt.state,
        &receipt.facts,
    ))?)
}
fn receipt_domain(state: &str) -> anyhow::Result<&'static [u8]> {
    match state {
        "pending" => Ok(b"prx.recovery.pending.v1"),
        "settled" => Ok(b"prx.recovery.settled.v1"),
        _ => anyhow::bail!("invalid recovery state"),
    }
}
fn sign(receipt: &mut RecoveryReceipt, key: &ring::hmac::Key) -> anyhow::Result<()> {
    receipt.mac = domain_mac(key, receipt_domain(&receipt.state)?, &receipt_bytes(receipt)?);
    Ok(())
}
fn active(conn: &Connection) -> anyhow::Result<(String, String)> {
    let (version, namespace, id): (i64, String, String) = conn.query_row(
        "SELECT version,backend_namespace,active_key_id FROM recovery_authority WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    anyhow::ensure!(
        version == 1 && uuid::Uuid::parse_str(&namespace)?.to_string() == namespace,
        "invalid recovery authority"
    );
    Ok((namespace, id))
}
fn stored(conn: &Connection, chain: &RecoveryChain) -> anyhow::Result<Option<RecoveryReceipt>> {
    validate_chain(chain)?;
    let namespace = active(conn)?.0;
    Ok(conn.query_row("SELECT version,key_id,state,facts,mac FROM recovery_attestations WHERE workspace_id=?1 AND owner_id=?2 AND source=?3 AND run_id=?4", params![chain.workspace_id,chain.owner_id,chain.source,chain.run_id], |r| Ok(RecoveryReceipt { version:r.get(0)?,backend_namespace:namespace,key_id:r.get(1)?,state:r.get(2)?,facts:r.get(3)?,mac:r.get(4)? })).optional()?)
}
fn verify(
    conn: &Connection,
    directory: &KeyDirectory,
    chain: &RecoveryChain,
    receipt: &RecoveryReceipt,
) -> anyhow::Result<BoundFacts> {
    anyhow::ensure!(
        receipt.version == 1 && receipt.facts.len() <= MAX_SERIALIZED_FACTS,
        "invalid recovery receipt"
    );
    let key = directory.load(&receipt.key_id)?;
    // ring verifies the domain-framed receipt in constant time.
    let domain = receipt_domain(&receipt.state)?;
    let bytes = receipt_bytes(receipt)?;
    ring::hmac::verify(&key, &framed(domain, &bytes), &receipt.mac)
        .map_err(|_| anyhow::anyhow!("invalid recovery authentication"))?;
    let facts: BoundFacts = serde_json::from_slice(&receipt.facts)?;
    anyhow::ensure!(
        facts.chain == *chain
            && ((receipt.state == "pending" && facts.input.is_some() && facts.terminal.is_none())
                || (receipt.state == "settled" && facts.terminal.is_some())),
        "recovery scope or state mismatch"
    );
    let references: (Option<String>, Option<String>) = conn.query_row(
        "SELECT input_event_id,terminal_event_id FROM recovery_attestations WHERE workspace_id=?1 AND owner_id=?2 AND source=?3 AND run_id=?4",
        params![chain.workspace_id,chain.owner_id,chain.source,chain.run_id], |r| Ok((r.get(0)?,r.get(1)?)))?;
    anyhow::ensure!(
        references.0.as_deref() == facts.input.as_ref().map(|e| e.id.as_str())
            && references.1.as_deref() == facts.terminal.as_ref().map(|e| e.id.as_str()),
        "recovery reference mismatch"
    );
    for bound in [&facts.input, &facts.terminal].into_iter().flatten() {
        let current = event(conn, chain, &bound.id)?;
        ring::hmac::verify(&key, &framed(b"prx.recovery.event.v1", &current.record), &bound.digest)
            .map_err(|_| anyhow::anyhow!("recovery evidence changed"))?;
    }
    Ok(facts)
}

// Recovery commits require SQLite FULL synchronization, independent of the
// ordinary memory writer's NORMAL policy. Restore the connection on all paths.
pub(super) struct FullSync<'a>(&'a Connection, i64);
impl<'a> FullSync<'a> {
    pub(super) fn new(conn: &'a Connection) -> anyhow::Result<Self> {
        let previous = conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        Ok(Self(conn, previous))
    }
}
impl Drop for FullSync<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.0.pragma_update(None, "synchronous", self.1) {
            tracing::error!(%error, "failed to restore SQLite synchronization");
        }
    }
}

fn same_hex(left: &str, right: &[u8]) -> bool {
    crate::memory::ingress_recovery::same_bytes(left.as_bytes(), hex::encode(right).as_bytes())
}

/// Whether this stable input key was accepted and later erased. The digest is
/// checked under every key id stored for the scope, so a rotated-out key still
/// matches; each comparison is `ring::hmac::verify`.
pub(super) fn tombstoned(
    conn: &Connection,
    directory: &KeyDirectory,
    scope: &IngressRecoveryScope,
    input_key: &str,
) -> anyhow::Result<bool> {
    let mut stmt = conn.prepare(
        "SELECT key_id,input_key_digest FROM recovery_tombstones WHERE workspace_id=?1 AND owner_id=?2 AND source=?3",
    )?;
    let rows = stmt
        .query_map(
            params![scope.workspace_id, scope.owner_id, scope.source.as_str()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let signed = framed(RecoveryMacDomain::InputKeyV3.bytes(), input_key.as_bytes());
    let mut keys: std::collections::HashMap<String, ring::hmac::Key> = std::collections::HashMap::new();
    for (key_id, digest) in rows {
        if !keys.contains_key(&key_id) {
            keys.insert(key_id.clone(), directory.load(&key_id)?);
        }
        if keys
            .get(&key_id)
            .is_some_and(|key| ring::hmac::verify(key, &signed, &digest).is_ok())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn same_fact(ok: bool, name: &'static str) -> anyhow::Result<()> {
    if ok {
        Ok(())
    } else {
        Err(IngressAttestationError::Mismatch(name).into())
    }
}

/// Write the pending attestation and its transport index row for an event this
/// transaction just inserted. Every fact is checked against the inserted event
/// and the capsule before anything is written.
pub(super) fn attest_ingress(
    tx: &rusqlite::Transaction<'_>,
    directory: &KeyDirectory,
    capsule: &IngressCapsule,
    attestation: &IngressAttestation,
    row: &MessageEvent,
) -> anyhow::Result<()> {
    let facts = PendingFacts::parse(&attestation.facts)?;
    facts.check_capsule(capsule)?;
    same_fact(facts.channel == row.channel, "channel")?;
    same_fact(facts.session_key == row.session_key, "session_key")?;
    same_fact(
        crate::runtime::turn_source::ingress_event_surface(&row.source, row.channel.as_deref(), &row.visibility)
            .is_some_and(|surface| surface.as_str() == facts.surface),
        "surface",
    )?;
    same_fact(facts.generation == row.config_generation_id.unwrap_or(0), "generation")?;
    same_fact(
        attestation.transport.key_id == facts.key_id && same_hex(&facts.transport_digest, &attestation.transport.tag),
        "transport_digest",
    )?;
    let key = directory.load(&facts.key_id)?;
    same_fact(
        attestation.input_key.key_id == facts.key_id
            && same_hex(&facts.input_key_digest, &attestation.input_key.tag)
            && ring::hmac::verify(
                &key,
                &framed(RecoveryMacDomain::InputKeyV3.bytes(), capsule.input_key.as_bytes()),
                &attestation.input_key.tag,
            )
            .is_ok(),
        "input_key_digest",
    )?;
    let chain = RecoveryChain {
        workspace_id: capsule.scope.workspace_id.clone(),
        owner_id: capsule.scope.owner_id.clone(),
        source: capsule.scope.source.as_str().to_owned(),
        run_id: capsule.run_id.clone(),
    };
    // The evidence is read back from the inserted row; `publish_in` rechecks it.
    let publication = RecoveryPublication {
        input: Some(event(tx, &chain, &row.event_id)?),
        chain,
        terminal: None,
        facts: attestation.facts.clone(),
    };
    check_publication(&publication, false)?;
    publish_in(tx, directory, &publication, false)?;
    tx.execute(
        "INSERT INTO recovery_transport_index(workspace_id,owner_id,source,run_id,transport_digest,input_key_digest,key_id) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            publication.chain.workspace_id,
            publication.chain.owner_id,
            publication.chain.source,
            publication.chain.run_id,
            attestation.transport.tag.as_slice(),
            attestation.input_key.tag.as_slice(),
            facts.key_id
        ],
    )?;
    Ok(())
}

impl SqliteMemory {
    /// The single shared warning for this handle; never repeated.
    fn warn_unavailable_once(&self) {
        if self.recovery_warned.set(()).is_ok() {
            tracing::warn!("recovery authority unavailable; activation will not be retried");
        }
    }

    /// Load a capsule's key. A key that cannot be loaded is remembered for this
    /// handle: later artifacts needing it fail at once, without a file read and
    /// without regeneration.
    /// Check, load and failure publication for one key id are serialized, so a
    /// queued caller never reloads a key after its failure was recorded.
    fn capsule_key(&self, id: &str) -> anyhow::Result<ring::hmac::Key> {
        // The index is always below the fixed length; a miss cannot occur.
        let _serial = self
            .recovery_key_gates
            .get(recovery_key_gate_index(id))
            .map(|gate| gate.lock());
        if self.recovery_unusable.lock().contains(id) {
            return Err(anyhow::Error::new(RecoveryKeyUnavailable(format!(
                "recovery key {id} unusable"
            ))));
        }
        let loaded = KeyDirectory::open(self.db_path.parent().context("missing memory directory")?, false)
            .and_then(|directory| directory.load(id));
        match loaded {
            Ok(key) => Ok(key),
            Err(error) => {
                self.recovery_unusable.lock().insert(id.to_owned());
                self.warn_unavailable_once();
                Err(key_unavailable(error))
            }
        }
    }

    fn recovery_activate(&self, rotate: bool) -> anyhow::Result<String> {
        let writer = self.pool.write();
        let sync = FullSync::new(&writer)?;
        let tx = rusqlite::Transaction::new_unchecked(&writer, TransactionBehavior::Immediate)?;
        let initialized: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='recovery_authority')",
            [],
            |r| r.get(0),
        )?;
        schema(&tx)?;
        let established: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM recovery_authority)", [], |r| r.get(0))?;
        anyhow::ensure!(!initialized || established, "missing established recovery authority");
        let directory = KeyDirectory::open(self.db_path.parent().context("missing memory directory")?, !established)
            .map_err(key_unavailable)?;
        let namespace = if established {
            let (namespace, id) = active(&tx)?;
            directory.load(&id).map_err(key_unavailable)?;
            // Every retained generation stays available, even during rotation.
            let mut stmt =
                tx.prepare("SELECT key_id FROM recovery_attestations UNION SELECT key_id FROM recovery_tombstones")?;
            for id in stmt.query_map([], |r| r.get::<_, String>(0))? {
                directory.load(&id?).map_err(key_unavailable)?;
            }
            drop(stmt);
            if !rotate {
                tx.commit()?;
                drop(sync);
                return Ok(id);
            }
            namespace
        } else {
            anyhow::ensure!(!rotate, "cannot rotate missing recovery authority");
            let history: i64 = tx.query_row("SELECT count(*) FROM recovery_attestations", [], |r| r.get(0))?;
            anyhow::ensure!(history == 0, "missing established recovery authority");
            uuid::Uuid::new_v4().to_string()
        };
        let id = directory.create().map_err(key_unavailable)?;
        directory.load(&id).map_err(key_unavailable)?;
        tx.execute("INSERT INTO recovery_authority(singleton,version,backend_namespace,active_key_id) VALUES(1,1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET active_key_id=excluded.active_key_id", params![namespace,id])?;
        tx.commit()?;
        drop(sync);
        // Retain every generation, including orphan files from failed commits.
        Ok(id)
    }

    fn recovery_publish(&self, publication: &RecoveryPublication, settle: bool) -> anyhow::Result<RecoveryReceipt> {
        check_publication(publication, settle)?;
        let writer = self.pool.write();
        let sync = FullSync::new(&writer)?;
        let tx = rusqlite::Transaction::new_unchecked(&writer, TransactionBehavior::Immediate)?;
        let directory = KeyDirectory::open(self.db_path.parent().context("missing memory directory")?, false)?;
        let receipt = publish_in(&tx, &directory, publication, settle)?;
        tx.commit()?;
        drop(sync);
        Ok(receipt)
    }
}

fn check_publication(publication: &RecoveryPublication, settle: bool) -> anyhow::Result<()> {
    validate_chain(&publication.chain)?;
    anyhow::ensure!(
        publication
            .input
            .as_ref()
            .zip(publication.terminal.as_ref())
            .is_none_or(|(input, terminal)| input.event_id != terminal.event_id),
        "input cannot be terminal evidence"
    );
    if publication.facts.len() > MAX_FACTS {
        return Err(RecoveryFactsTooLarge { limit: MAX_FACTS }.into());
    }
    anyhow::ensure!(settle == publication.terminal.is_some(), "invalid recovery publication");
    anyhow::ensure!(settle || publication.input.is_some(), "pending reception needs input");
    Ok(())
}

/// Publish inside the caller's transaction. The caller owns `FullSync` and the
/// commit, so a combined transaction (ingress append + attestation) stays atomic.
fn publish_in(
    tx: &rusqlite::Transaction<'_>,
    directory: &KeyDirectory,
    publication: &RecoveryPublication,
    settle: bool,
) -> anyhow::Result<RecoveryReceipt> {
    // Publication is load-only: activation is a separate explicit operation.
    let (namespace, active_id) = active(tx)?;
    let previous = stored(tx, &publication.chain)?;
    let key_id = previous
        .as_ref()
        .filter(|r| !settle || r.state == "settled")
        .map_or(active_id, |r| r.key_id.clone());
    let key = directory.load(&key_id)?;
    let input = publication
        .input
        .as_ref()
        .map(|e| bind(tx, &publication.chain, e, &key))
        .transpose()?;
    let terminal = publication
        .terminal
        .as_ref()
        .map(|e| bind(tx, &publication.chain, e, &key))
        .transpose()?;

    let terminal_bytes = serde_json::to_vec(&terminal)?;
    let old_facts = previous
        .as_ref()
        .map(|r| verify(tx, directory, &publication.chain, r))
        .transpose()?;
    if let Some(old) = &old_facts {
        anyhow::ensure!(
            old.input.as_ref().map(|e| &e.id) == input.as_ref().map(|e| &e.id),
            "conflicting recovery input"
        );
        if !settle || previous.as_ref().is_some_and(|r| r.state == "settled") {
            anyhow::ensure!(
                if settle {
                    serde_json::to_vec(&old.terminal)? == terminal_bytes && old.completion == publication.facts
                } else {
                    old.pending == publication.facts
                },
                "conflicting recovery publication"
            );
            return previous.context("missing prior receipt");
        }
    } else {
        anyhow::ensure!(!settle || input.is_none(), "settlement requires pending reception");
    }
    let facts = BoundFacts {
        chain: publication.chain.clone(),
        input,
        terminal,
        pending: if settle {
            old_facts.map_or_else(Vec::new, |f| f.pending)
        } else {
            publication.facts.clone()
        },
        completion: if settle { publication.facts.clone() } else { Vec::new() },
    };
    let mut receipt = RecoveryReceipt {
        version: 1,
        backend_namespace: namespace,
        key_id,
        state: if settle { "settled" } else { "pending" }.into(),
        facts: serde_json::to_vec(&facts)?,
        mac: Vec::new(),
    };
    if receipt.facts.len() > MAX_SERIALIZED_FACTS {
        return Err(RecoveryFactsTooLarge {
            limit: MAX_SERIALIZED_FACTS,
        }
        .into());
    }
    sign(&mut receipt, &key)?;
    tx.execute("INSERT INTO recovery_attestations(workspace_id,owner_id,source,run_id,input_event_id,terminal_event_id,version,key_id,state,facts,mac) VALUES(?1,?2,?3,?4,?5,?6,1,?7,?8,?9,?10) ON CONFLICT(workspace_id,owner_id,source,run_id) DO UPDATE SET terminal_event_id=excluded.terminal_event_id,key_id=excluded.key_id,state=excluded.state,facts=excluded.facts,mac=excluded.mac", params![publication.chain.workspace_id,publication.chain.owner_id,publication.chain.source,publication.chain.run_id,facts.input.as_ref().map(|e| &e.id),facts.terminal.as_ref().map(|e| &e.id),receipt.key_id,receipt.state,receipt.facts,receipt.mac])?;
    Ok(receipt)
}

impl RecoveryRepository for SqliteMemory {
    fn activate_recovery_authority(&self) -> anyhow::Result<String> {
        // Unavailable key material is sticky for this handle: never retried or
        // regenerated. A successful activation is not memoized, so every call
        // re-loads the active key and refuses one lost or corrupted afterwards.
        // The gate covers the check, the load and the failure publication, so
        // queued callers observe the recorded failure instead of loading again.
        let _serial = self.recovery_activation.lock();
        if self.recovery_unavailable.get().is_some() {
            return Err(anyhow::Error::new(RecoveryKeyUnavailable(
                "recovery authority unavailable; activation was refused earlier".to_owned(),
            )));
        }
        #[cfg(test)]
        self.recovery_activation_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.recovery_activate(false) {
            Ok(id) => Ok(id),
            Err(error) if crate::memory::recovery_authority::is_key_unavailable(&error) => {
                let _ = self.recovery_unavailable.set(());
                self.warn_unavailable_once();
                Err(error)
            }
            Err(error) => Err(error),
        }
    }
    fn recovery_mac(
        &self,
        key_id: Option<&str>,
        domain: RecoveryMacDomain,
        bytes: &[u8],
    ) -> anyhow::Result<RecoveryTag> {
        anyhow::ensure!(bytes.len() <= MAX_SERIALIZED_FACTS, "oversized recovery MAC input");
        let writer = self.pool.write();
        let id = match key_id {
            Some(id) => id.to_owned(),
            None => active(&writer)?.1,
        };
        let key = self.capsule_key(&id)?;
        let signed = ring::hmac::sign(&key, &framed(domain.bytes(), bytes));
        let tag = signed.as_ref().try_into().context("invalid recovery MAC length")?;
        Ok(RecoveryTag { key_id: id, tag })
    }
    fn verify_recovery_mac(&self, tag: &RecoveryTag, domain: RecoveryMacDomain, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(bytes.len() <= MAX_SERIALIZED_FACTS, "oversized recovery MAC input");
        let key = self.capsule_key(&tag.key_id)?;
        ring::hmac::verify(&key, &framed(domain.bytes(), bytes), &tag.tag)
            .map_err(|_| anyhow::anyhow!("invalid recovery authentication"))
    }
    fn rotate_recovery_authority(&self) -> anyhow::Result<String> {
        self.recovery_activate(true)
    }
    fn recovery_event_evidence(&self, chain: &RecoveryChain, id: &str) -> anyhow::Result<RecoveryEvidence> {
        event(&self.pool.write(), chain, id)
    }
    fn record_recovery_pending(&self, publication: &RecoveryPublication) -> anyhow::Result<RecoveryReceipt> {
        self.recovery_publish(publication, false)
    }
    fn settle_recovery(&self, publication: &RecoveryPublication) -> anyhow::Result<RecoveryReceipt> {
        self.recovery_publish(publication, true)
    }
    fn read_recovery_receipt(&self, chain: &RecoveryChain) -> anyhow::Result<Option<RecoveryReceipt>> {
        let writer = self.pool.write();
        let tx = writer.unchecked_transaction()?;
        let directory = KeyDirectory::open(self.db_path.parent().context("missing memory directory")?, false)?;
        let receipt = stored(&tx, chain)?;
        if let Some(receipt) = &receipt {
            verify(&tx, &directory, chain, receipt)?;
        }
        tx.commit()?;
        Ok(receipt)
    }
}

#[cfg(all(test, unix))]
#[path = "tests/recovery_authority.rs"]
mod tests;
