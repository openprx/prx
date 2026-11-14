//! Golden-vector access to the production repository encoders (Part-2 D§4.5).
//! Every function delegates to the production helper; nothing is re-derived.
use super::SqliteMemory;
use super::recovery_keys::KeyDirectory;
use crate::memory::recovery_authority::{RecoveryCostState, RecoveryPricing, RecoveryReceipt};
use anyhow::Context;

fn directory(memory: &SqliteMemory) -> anyhow::Result<KeyDirectory> {
    KeyDirectory::open(memory.db_path.parent().context("missing memory directory")?, true)
}

/// Install `bytes` under its content-derived key id and return that id.
pub(crate) fn install_key(memory: &SqliteMemory, bytes: &[u8; 32]) -> anyhow::Result<String> {
    let id = KeyDirectory::key_id(bytes);
    directory(memory)?.install(&id, bytes)?;
    Ok(id)
}

/// E3 over explicit column values.
pub(crate) fn row_codec(values: &[rusqlite::types::Value]) -> anyhow::Result<Vec<u8>> {
    let mut record = Vec::new();
    for value in values {
        anyhow::ensure!(
            super::recovery_authority::push_column(&mut record, rusqlite::types::ValueRef::from(value)),
            "record bound"
        );
    }
    Ok(record)
}

/// E4 under the installed key.
pub(crate) fn event_digest(memory: &SqliteMemory, key_id: &str, record: &[u8]) -> anyhow::Result<Vec<u8>> {
    let key = directory(memory)?.load(key_id)?;
    Ok(super::recovery_authority::event_digest(&key, record))
}

/// E5 under the installed key.
pub(crate) fn receipt_mac(
    memory: &SqliteMemory,
    key_id: &str,
    backend_namespace: &str,
    state: &str,
    facts: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let key = directory(memory)?.load(key_id)?;
    let mut receipt = RecoveryReceipt {
        version: 1,
        backend_namespace: backend_namespace.to_owned(),
        key_id: key_id.to_owned(),
        state: state.to_owned(),
        facts: facts.to_vec(),
        mac: Vec::new(),
    };
    super::recovery_authority::sign(&mut receipt, &key)?;
    Ok(receipt.mac)
}

/// E7a and E7b under the installed key; `identity` is
/// `[workspace_id, owner_id, settlement_id]`.
pub(crate) fn outbox_macs(
    memory: &SqliteMemory,
    key_id: &str,
    identity: [&str; 3],
    usage_digest: &[u8; 32],
    facts: &[u8],
    state: RecoveryCostState,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let [workspace_id, owner_id, settlement_id] = identity;
    let key = directory(memory)?.load(key_id)?;
    Ok((
        super::recovery_outbox::sign_outbox(
            &key,
            workspace_id,
            owner_id,
            settlement_id,
            1,
            key_id,
            usage_digest,
            RecoveryPricing::Metered,
            facts,
        )?,
        super::recovery_outbox::sign_state(&key, workspace_id, owner_id, settlement_id, usage_digest, state)?,
    ))
}
