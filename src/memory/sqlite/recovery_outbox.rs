//! Authenticated recovery cost outbox. Projection (3b) is the only writer of state.

use super::{SqliteMemory, recovery_authority, recovery_keys::KeyDirectory};
use crate::memory::recovery_authority::{RecoveryCostRow, RecoveryCostState, RecoveryMacDomain, RecoveryPricing};
use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

const OUTBOX_FACT_KEYS: [&str; 7] = [
    "v",
    "terminal_id",
    "status",
    "pricing",
    "usage",
    "usage_event_id",
    "final_event_id",
];

#[cfg(test)]
thread_local! {
    static SKIP_OUTBOX_MAC_VERIFY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn skip_outbox_mac_verify(skip: bool) {
    SKIP_OUTBOX_MAC_VERIFY.with(|cell| cell.set(skip));
}

pub(super) fn schema(conn: &Connection) -> anyhow::Result<()> {
    let max_facts = recovery_authority::MAX_SERIALIZED_FACTS;
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS recovery_cost_outbox (
            workspace_id TEXT NOT NULL,
            owner_id TEXT NOT NULL,
            settlement_id TEXT NOT NULL,
            version INTEGER NOT NULL CHECK(version=1),
            key_id TEXT NOT NULL,
            usage_digest BLOB NOT NULL CHECK(length(usage_digest)=32),
            pricing TEXT NOT NULL CHECK(pricing IN ('metered','subscription','unknown')),
            state TEXT NOT NULL CHECK(state IN ('pending','acknowledged','error','blocked')),
            facts BLOB NOT NULL CHECK(length(facts)<={max_facts}),
            mac BLOB NOT NULL CHECK(length(mac)=32),
            state_mac BLOB NOT NULL CHECK(length(state_mac)=32),
            UNIQUE(workspace_id, settlement_id)
        );
        CREATE TRIGGER IF NOT EXISTS recovery_cost_outbox_state_only BEFORE UPDATE ON recovery_cost_outbox
        WHEN OLD.workspace_id IS NOT NEW.workspace_id
             OR OLD.owner_id IS NOT NEW.owner_id
             OR OLD.settlement_id IS NOT NEW.settlement_id
             OR OLD.version IS NOT NEW.version
             OR OLD.key_id IS NOT NEW.key_id
             OR OLD.usage_digest IS NOT NEW.usage_digest
             OR OLD.pricing IS NOT NEW.pricing
             OR OLD.facts IS NOT NEW.facts
             OR OLD.mac IS NOT NEW.mac
             OR OLD.state NOT IN ('pending','error')
             OR NEW.state NOT IN ('acknowledged','error','blocked')
        BEGIN SELECT RAISE(ABORT,'illegal recovery cost outbox transition'); END;"
    ))?;
    Ok(())
}

pub(super) fn insert_in(
    tx: &rusqlite::Transaction<'_>,
    directory: &KeyDirectory,
    workspace_id: &str,
    owner_id: &str,
    settlement_id: &str,
    key_id: &str,
    pricing: RecoveryPricing,
    facts: &[u8],
) -> anyhow::Result<InsertOutbox> {
    validate_identity(workspace_id, "workspace_id")?;
    validate_identity(owner_id, "owner_id")?;
    validate_identity(settlement_id, "settlement_id")?;
    anyhow::ensure!(
        !key_id.is_empty() && key_id.len() <= 1024,
        "recovery cost outbox key_id is invalid"
    );
    anyhow::ensure!(
        facts.len() <= recovery_authority::MAX_SERIALIZED_FACTS,
        "recovery cost outbox facts are invalid"
    );
    validate_facts(facts, pricing)?;
    let state = if pricing == RecoveryPricing::Metered {
        RecoveryCostState::Pending
    } else {
        RecoveryCostState::Acknowledged
    };
    let key = directory.load(key_id)?;
    let usage_digest = usage_digest_mac(&key, facts)?;
    let mac = sign_outbox(
        &key,
        workspace_id,
        owner_id,
        settlement_id,
        1,
        key_id,
        &usage_digest,
        pricing,
        facts,
    )?;
    let state_mac = sign_state(&key, workspace_id, owner_id, settlement_id, &usage_digest, state)?;
    let existing = tx
        .query_row(
            "SELECT workspace_id, owner_id, settlement_id, version, key_id, usage_digest,
                pricing, state, facts, mac, state_mac FROM recovery_cost_outbox
         WHERE workspace_id=?1 AND settlement_id=?2",
            params![workspace_id, settlement_id],
            read_raw,
        )
        .optional()?;
    if let Some(raw) = existing {
        // Projection may advance the state. Authenticate that recorded state
        // rather than comparing it with a newly generated pending state.
        let stored = authenticate(directory, raw)?;
        if stored.owner_id != owner_id || stored.facts != facts || stored.key_id != key_id || stored.pricing != pricing
        {
            return Err(crate::memory::recovery_authority::RecoveryTerminalConflict.into());
        }
        return Ok(InsertOutbox::Replayed);
    }
    tx.execute(
        "INSERT INTO recovery_cost_outbox(
            workspace_id, owner_id, settlement_id, version, key_id, usage_digest, pricing, state, facts, mac, state_mac
         ) VALUES(?1,?2,?3,1,?4,?5,?6,?7,?8,?9,?10)",
        params![
            workspace_id,
            owner_id,
            settlement_id,
            key_id,
            usage_digest.as_slice(),
            pricing.as_str(),
            state.as_str(),
            facts,
            mac,
            state_mac
        ],
    )?;
    Ok(InsertOutbox::Inserted)
}

pub(super) enum InsertOutbox {
    Inserted,
    Replayed,
}

pub(super) fn pending_recovery_costs(
    memory: &SqliteMemory,
    workspace_id: &str,
    after: Option<&str>,
    limit: usize,
) -> anyhow::Result<Vec<RecoveryCostRow>> {
    validate_identity(workspace_id, "workspace_id")?;
    anyhow::ensure!(limit > 0, "recovery cost outbox page limit is invalid");
    let limit = limit.min(crate::memory::ingress_recovery::MAX_RECOVERY_PAGE);
    let writer = memory.pool.write();
    let present: bool = writer.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='recovery_cost_outbox' AND type='table')",
        [],
        |row| row.get(0),
    )?;
    if !present {
        return Ok(Vec::new());
    }
    let directory = KeyDirectory::open(memory.db_path.parent().context("missing memory directory")?, false)?;
    let mut statement = writer.prepare(
        "SELECT workspace_id, owner_id, settlement_id, version, key_id, usage_digest, pricing, state, facts, mac, state_mac
         FROM recovery_cost_outbox
         WHERE workspace_id=?1 AND state IN ('pending','error')
           AND (?2 IS NULL OR settlement_id > ?2)
         ORDER BY settlement_id ASC
         LIMIT ?3",
    )?;
    let rows = statement.query_map(params![workspace_id, after, limit as i64], read_raw)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(authenticate(&directory, row?)?);
    }
    Ok(out)
}

pub(super) fn finish_recovery_cost(
    memory: &SqliteMemory,
    workspace_id: &str,
    settlement_id: &str,
    usage_digest: &[u8; 32],
    state: RecoveryCostState,
) -> anyhow::Result<()> {
    validate_identity(workspace_id, "workspace_id")?;
    validate_identity(settlement_id, "settlement_id")?;
    anyhow::ensure!(
        matches!(
            state,
            RecoveryCostState::Acknowledged | RecoveryCostState::Error | RecoveryCostState::Blocked
        ),
        "recovery cost outbox state is invalid"
    );
    let writer = memory.pool.write();
    let tx = rusqlite::Transaction::new_unchecked(&writer, TransactionBehavior::Immediate)?;
    let raw = tx.query_row(
        "SELECT workspace_id, owner_id, settlement_id, version, key_id, usage_digest,
                pricing, state, facts, mac, state_mac FROM recovery_cost_outbox
         WHERE workspace_id=?1 AND settlement_id=?2",
        params![workspace_id, settlement_id],
        read_raw,
    )?;
    let directory = KeyDirectory::open(memory.db_path.parent().context("missing memory directory")?, false)?;
    let row = authenticate(&directory, raw)?;
    if row.state == state {
        return Ok(());
    }
    anyhow::ensure!(
        matches!(row.state, RecoveryCostState::Pending | RecoveryCostState::Error),
        "illegal recovery cost outbox transition"
    );
    let owner_id = row.owner_id;
    let facts = row.facts;
    let key = directory.load(&row.key_id)?;
    let usage = usage_bytes(&facts)?;
    ring::hmac::verify(
        &key,
        &recovery_authority::framed(RecoveryMacDomain::UsageDigestV1.bytes(), &usage),
        usage_digest,
    )
    .map_err(|_| anyhow::anyhow!("recovery cost outbox usage_digest is invalid"))?;
    let state_mac = sign_state(&key, workspace_id, &owner_id, settlement_id, usage_digest, state)?;
    tx.execute(
        "UPDATE recovery_cost_outbox SET state=?1, state_mac=?2 WHERE workspace_id=?3 AND settlement_id=?4",
        params![state.as_str(), state_mac, workspace_id, settlement_id],
    )?;
    tx.commit()?;
    Ok(())
}

struct RawOutbox {
    workspace_id: String,
    owner_id: String,
    settlement_id: String,
    version: i64,
    key_id: String,
    usage_digest: Vec<u8>,
    pricing: String,
    state: String,
    facts: Vec<u8>,
    mac: Vec<u8>,
    state_mac: Vec<u8>,
}

fn authenticate(directory: &KeyDirectory, raw: RawOutbox) -> anyhow::Result<RecoveryCostRow> {
    validate_identity(&raw.workspace_id, "workspace_id")?;
    validate_identity(&raw.owner_id, "owner_id")?;
    validate_identity(&raw.settlement_id, "settlement_id")?;
    anyhow::ensure!(raw.version == 1, "recovery cost outbox version is invalid");
    anyhow::ensure!(
        !raw.key_id.is_empty() && raw.key_id.len() <= 1024,
        "recovery cost outbox key_id is invalid"
    );
    let usage_digest: [u8; 32] = raw
        .usage_digest
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("recovery cost outbox usage_digest is invalid"))?;
    let pricing = RecoveryPricing::parse(&raw.pricing)?;
    let state = RecoveryCostState::parse(&raw.state)?;
    anyhow::ensure!(
        raw.facts.len() <= recovery_authority::MAX_SERIALIZED_FACTS,
        "recovery cost outbox facts are invalid"
    );
    validate_facts(&raw.facts, pricing)?;
    let mac: [u8; 32] = raw
        .mac
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("recovery cost outbox mac is invalid"))?;
    let state_mac: [u8; 32] = raw
        .state_mac
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("recovery cost outbox state_mac is invalid"))?;
    let key = directory.load(&raw.key_id)?;
    let skip_mac = {
        #[cfg(test)]
        {
            SKIP_OUTBOX_MAC_VERIFY.with(std::cell::Cell::get)
        }
        #[cfg(not(test))]
        {
            false
        }
    };
    if !skip_mac {
        verify_outbox_mac(
            &key,
            &raw.workspace_id,
            &raw.owner_id,
            &raw.settlement_id,
            raw.version,
            &raw.key_id,
            &usage_digest,
            pricing,
            &raw.facts,
            &mac,
        )?;
    }
    verify_state_mac(
        &key,
        &raw.workspace_id,
        &raw.owner_id,
        &raw.settlement_id,
        &usage_digest,
        state,
        &state_mac,
    )?;
    ring::hmac::verify(
        &key,
        &recovery_authority::framed(RecoveryMacDomain::UsageDigestV1.bytes(), &usage_bytes(&raw.facts)?),
        &usage_digest,
    )
    .map_err(|_| anyhow::anyhow!("recovery cost outbox usage_digest is invalid"))?;
    Ok(RecoveryCostRow {
        workspace_id: raw.workspace_id,
        owner_id: raw.owner_id,
        settlement_id: raw.settlement_id,
        version: raw.version,
        key_id: raw.key_id,
        usage_digest,
        pricing,
        state,
        facts: raw.facts,
    })
}

fn verify_outbox_mac(
    key: &ring::hmac::Key,
    workspace_id: &str,
    owner_id: &str,
    settlement_id: &str,
    version: i64,
    key_id: &str,
    usage_digest: &[u8; 32],
    pricing: RecoveryPricing,
    facts: &[u8],
    tag: &[u8; 32],
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(&serde_json::json!({
        "workspace": workspace_id,
        "owner": owner_id,
        "settlement_id": settlement_id,
        "version": version,
        "key_id": key_id,
        "usage_digest": usage_digest.as_slice(),
        "pricing": pricing.as_str(),
        "facts": facts,
    }))?;
    ring::hmac::verify(
        key,
        &recovery_authority::framed(RecoveryMacDomain::OutboxV1.bytes(), &body),
        tag,
    )
    .map_err(|_| anyhow::anyhow!("recovery cost outbox mac is invalid"))
}

fn verify_state_mac(
    key: &ring::hmac::Key,
    workspace_id: &str,
    owner_id: &str,
    settlement_id: &str,
    usage_digest: &[u8; 32],
    state: RecoveryCostState,
    tag: &[u8; 32],
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(&serde_json::json!({
        "workspace": workspace_id,
        "owner": owner_id,
        "settlement_id": settlement_id,
        "usage_digest": usage_digest.as_slice(),
        "state": state.as_str(),
    }))?;
    ring::hmac::verify(
        key,
        &recovery_authority::framed(RecoveryMacDomain::OutboxStateV1.bytes(), &body),
        tag,
    )
    .map_err(|_| anyhow::anyhow!("recovery cost outbox state_mac is invalid"))
}

fn usage_digest_mac(key: &ring::hmac::Key, facts: &[u8]) -> anyhow::Result<[u8; 32]> {
    let bytes = usage_bytes(facts)?;
    let tag = ring::hmac::sign(
        key,
        &recovery_authority::framed(RecoveryMacDomain::UsageDigestV1.bytes(), &bytes),
    );
    tag.as_ref()
        .try_into()
        .context("recovery cost outbox usage_digest is invalid")
}

fn sign_outbox(
    key: &ring::hmac::Key,
    workspace_id: &str,
    owner_id: &str,
    settlement_id: &str,
    version: i64,
    key_id: &str,
    usage_digest: &[u8; 32],
    pricing: RecoveryPricing,
    facts: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::to_vec(&serde_json::json!({
        "workspace": workspace_id,
        "owner": owner_id,
        "settlement_id": settlement_id,
        "version": version,
        "key_id": key_id,
        "usage_digest": usage_digest.as_slice(),
        "pricing": pricing.as_str(),
        "facts": facts,
    }))?;
    Ok(ring::hmac::sign(
        key,
        &recovery_authority::framed(RecoveryMacDomain::OutboxV1.bytes(), &body),
    )
    .as_ref()
    .to_vec())
}

fn sign_state(
    key: &ring::hmac::Key,
    workspace_id: &str,
    owner_id: &str,
    settlement_id: &str,
    usage_digest: &[u8; 32],
    state: RecoveryCostState,
) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::to_vec(&serde_json::json!({
        "workspace": workspace_id,
        "owner": owner_id,
        "settlement_id": settlement_id,
        "usage_digest": usage_digest.as_slice(),
        "state": state.as_str(),
    }))?;
    Ok(ring::hmac::sign(
        key,
        &recovery_authority::framed(RecoveryMacDomain::OutboxStateV1.bytes(), &body),
    )
    .as_ref()
    .to_vec())
}

fn validate_identity(value: &str, field: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= 1024,
        "recovery cost outbox {field} is invalid"
    );
    Ok(())
}

fn validate_facts(facts: &[u8], pricing: RecoveryPricing) -> anyhow::Result<()> {
    let value: serde_json::Value =
        serde_json::from_slice(facts).map_err(|_| anyhow::anyhow!("recovery cost outbox facts are invalid"))?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("recovery cost outbox facts are invalid"))?;
    if object.keys().any(|key| !OUTBOX_FACT_KEYS.contains(&key.as_str())) {
        anyhow::bail!("recovery cost outbox facts contain an unknown key");
    }
    if OUTBOX_FACT_KEYS.iter().any(|key| !object.contains_key(*key)) {
        anyhow::bail!("recovery cost outbox facts are invalid");
    }
    let v = object
        .get("v")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("recovery cost outbox facts are invalid"))?;
    anyhow::ensure!(v == 1, "recovery cost outbox facts are invalid");
    let stored = object
        .get("pricing")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("recovery cost outbox facts are invalid"))?;
    anyhow::ensure!(stored == pricing.as_str(), "recovery cost outbox pricing is invalid");
    Ok(())
}

/// Test helper: insert a fully formed row, bypassing MAC construction.
#[cfg(test)]
pub(crate) fn insert_raw_for_test(
    memory: &SqliteMemory,
    workspace_id: &str,
    owner_id: &str,
    settlement_id: &str,
    version: i64,
    key_id: &str,
    usage_digest: &[u8],
    pricing: &str,
    state: &str,
    facts: &[u8],
    mac: &[u8],
    state_mac: &[u8],
) -> anyhow::Result<()> {
    schema(&memory.pool.write())?;
    memory.pool.write().execute(
        "INSERT INTO recovery_cost_outbox(
            workspace_id, owner_id, settlement_id, version, key_id, usage_digest, pricing, state, facts, mac, state_mac
         ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            workspace_id,
            owner_id,
            settlement_id,
            version,
            key_id,
            usage_digest,
            pricing,
            state,
            facts,
            mac,
            state_mac
        ],
    )?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn usage_digest_for_test(memory: &SqliteMemory, key_id: &str, facts: &[u8]) -> anyhow::Result<[u8; 32]> {
    let directory = KeyDirectory::open(memory.db_path.parent().context("missing memory directory")?, false)?;
    let key = directory.load(key_id)?;
    usage_digest_mac(&key, facts)
}

#[cfg(test)]
pub(crate) fn sign_outbox_for_test(
    memory: &SqliteMemory,
    workspace_id: &str,
    owner_id: &str,
    settlement_id: &str,
    key_id: &str,
    usage_digest: &[u8; 32],
    pricing: RecoveryPricing,
    facts: &[u8],
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let directory = KeyDirectory::open(memory.db_path.parent().context("missing memory directory")?, false)?;
    let key = directory.load(key_id)?;
    let state = if pricing == RecoveryPricing::Metered {
        RecoveryCostState::Pending
    } else {
        RecoveryCostState::Acknowledged
    };
    Ok((
        sign_outbox(
            &key,
            workspace_id,
            owner_id,
            settlement_id,
            1,
            key_id,
            usage_digest,
            pricing,
            facts,
        )?,
        sign_state(&key, workspace_id, owner_id, settlement_id, usage_digest, state)?,
    ))
}

fn read_raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawOutbox> {
    Ok(RawOutbox {
        workspace_id: row.get(0)?,
        owner_id: row.get(1)?,
        settlement_id: row.get(2)?,
        version: row.get(3)?,
        key_id: row.get(4)?,
        usage_digest: row.get(5)?,
        pricing: row.get(6)?,
        state: row.get(7)?,
        facts: row.get(8)?,
        mac: row.get(9)?,
        state_mac: row.get(10)?,
    })
}

fn usage_bytes(facts: &[u8]) -> anyhow::Result<Vec<u8>> {
    let value: serde_json::Value = serde_json::from_slice(facts).context("recovery cost outbox facts are invalid")?;
    let usage: Option<crate::llm::route_decision::MeteredTokenUsageRecord> = serde_json::from_value(
        value
            .get("usage")
            .cloned()
            .context("recovery cost outbox usage is missing")?,
    )?;
    Ok(serde_json::to_vec(&usage)?)
}

#[cfg(test)]
pub(crate) fn forge_acknowledged_cost_for_test(workspace: &std::path::Path, settlement: &str) -> anyhow::Result<()> {
    let memory = SqliteMemory::new(workspace)?;
    let changed = memory.pool.write().execute(
        "UPDATE recovery_cost_outbox SET state='acknowledged' WHERE settlement_id=?1 AND state='pending'",
        [settlement],
    )?;
    anyhow::ensure!(changed == 1, "test: expected one pending outbox row");
    Ok(())
}

#[cfg(test)]
pub(crate) fn forge_error_cost_for_test(workspace: &std::path::Path, settlement: &str) -> anyhow::Result<()> {
    let memory = SqliteMemory::new(workspace)?;
    let changed = memory.pool.write().execute(
        "UPDATE recovery_cost_outbox SET state='error' WHERE settlement_id=?1 AND state='pending'",
        [settlement],
    )?;
    anyhow::ensure!(changed == 1, "test: expected one pending outbox row");
    Ok(())
}

#[cfg(test)]
pub(crate) fn remove_cost_for_test(workspace: &std::path::Path, settlement: &str) -> anyhow::Result<()> {
    let memory = SqliteMemory::new(workspace)?;
    let changed = memory
        .pool
        .write()
        .execute("DELETE FROM recovery_cost_outbox WHERE settlement_id=?1", [settlement])?;
    anyhow::ensure!(changed == 1, "test: expected one outbox row to remove");
    Ok(())
}
