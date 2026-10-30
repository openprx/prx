//! Physical delivery references. Adoption eligibility and retention stay in A.
use super::experiment::ExperimentRunBinding;
use super::generation::{StoreLock, ensure_dir_synced};
use super::store::{check_private, record_bytes};
use super::{LessonRef, ModelBinding, OwnerScope, PreparedLessonContext, SkillId, SkillLifecycleStore};
use crate::decision::{DecisionKey, KeyDomain};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

const DIRECTORY: &str = "lesson-deliveries";
const MAX_RECORD_BYTES: u64 = 16 * 1024;

#[cfg(test)]
mod tests;

pub(crate) struct LessonPhysicalIdentity {
    pub(crate) turn_id: String,
    pub(crate) attempt_id: String,
    pub(crate) binding_sha256: String,
}

pub(crate) struct LessonDeliveryRequest {
    pub(crate) actual_model: ModelBinding,
    pub(crate) physical: LessonPhysicalIdentity,
    pub(crate) body_binding_sha256: String,
    pub(crate) final_context_sha256: String,
    pub(crate) deadline: Instant,
}

/// A may use its own reduced frozen work allowance; B never raises it.
pub(super) struct DeliveryRetentionLimits {
    pub(super) max_records: usize,
    pub(super) max_decoded_bytes: u64,
    pub(super) deadline: Instant,
}

pub(super) struct DeliveryRetentionRefs {
    pub(super) refs: Vec<LessonRef>,
    pub(super) examined_records: usize,
    pub(super) decoded_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("lesson delivery persistence was not confirmed; transport is refused")]
pub(crate) struct LessonDeliveryWriteUnconfirmed;

#[derive(Clone, Copy, Serialize, Deserialize)]
enum DeliveryState {
    ProvenUnsent,
    PossibleHandoffUnknown,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    schema_version: u32,
    delivery_id: String,
    owner_scope: OwnerScope,
    skill_id: SkillId,
    state: DeliveryState,
    turn_id: String,
    attempt_id: String,
    physical_binding_sha256: String,
    snapshot_sha256: String,
    block_sha256: String,
    final_context_sha256: String,
    body_binding_sha256: String,
    experiment: ExperimentRunBinding,
    model: ModelBinding,
    refs: Vec<LessonRef>,
    created_at: DateTime<Utc>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedDelivery {
    payload: Payload,
    seal: String,
}

pub(crate) fn persist_lesson_delivery(
    prepared: &PreparedLessonContext,
    request: &LessonDeliveryRequest,
    key: &DecisionKey,
    cancel: &CancellationToken,
) -> Result<()> {
    active(cancel, request.deadline)?;
    let guard = prepared.lock_handoff(&request.actual_model, Utc::now())?;
    let store = guard.store();
    let skill = guard.skill_id();
    let remaining = request.deadline.saturating_duration_since(Instant::now());
    let _skill = store.lock_skill(skill, remaining.min(super::PUBLICATION_LOCK_TIMEOUT))?;
    active(cancel, request.deadline)?;
    let physical = &request.physical;
    for id in [&physical.turn_id, &physical.attempt_id] {
        ensure!(
            !id.is_empty() && id.len() <= 256 && id.bytes().all(|byte| byte.is_ascii_graphic()),
            "invalid physical lesson identity"
        );
    }
    ensure!(
        [
            physical.binding_sha256.as_str(),
            request.body_binding_sha256.as_str(),
            request.final_context_sha256.as_str(),
            prepared.snapshot_sha256(),
            prepared.block_sha256(),
        ]
        .iter()
        .all(|digest| valid_digest(digest)),
        "lesson delivery has an invalid digest"
    );
    ensure!(
        request.final_context_sha256 == prepared.expected_user_slot_sha256(),
        "lesson final context pin changed"
    );
    ensure!(
        !prepared.refs().is_empty() && prepared.refs().len() <= 2,
        "lesson delivery references exceed the frozen context"
    );
    let store_key = existing_store_key(store)?;
    let key_parts: &[&[u8]] = &[b"lesson-delivery-owner-key-v1", store.owner_scope().as_str().as_bytes()];
    ensure!(
        store_key.verify_hmac(
            KeyDomain::ReportOwnership,
            key_parts,
            &key.hmac(KeyDomain::ReportOwnership, key_parts)
        ),
        "lesson physical accounting key belongs to another workspace"
    );
    active(cancel, request.deadline)?;
    let id = delivery_id(key, store.owner_scope(), &physical.turn_id, &physical.attempt_id);
    let dir = store.root().join(DIRECTORY);
    let path = dir.join(format!("{id}.json"));
    // Existing IDs cannot authorize a second handoff or an automatic replay.
    match fs::symlink_metadata(&path) {
        Ok(_) => anyhow::bail!("lesson physical attempt already has delivery evidence"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context(LessonDeliveryWriteUnconfirmed),
    }
    let mut record = SealedDelivery {
        payload: Payload {
            schema_version: 1,
            delivery_id: id,
            owner_scope: store.owner_scope().clone(),
            skill_id: skill.clone(),
            state: DeliveryState::ProvenUnsent,
            turn_id: physical.turn_id.clone(),
            attempt_id: physical.attempt_id.clone(),
            physical_binding_sha256: physical.binding_sha256.clone(),
            snapshot_sha256: prepared.snapshot_sha256().to_owned(),
            block_sha256: prepared.block_sha256().to_owned(),
            final_context_sha256: request.final_context_sha256.clone(),
            body_binding_sha256: request.body_binding_sha256.clone(),
            experiment: prepared.binding().clone(),
            model: request.actual_model.clone(),
            refs: prepared.refs().to_vec(),
            created_at: Utc::now(),
        },
        seal: String::new(),
    };
    // Directory creation is durable mutation too. Its uncertainty must remain
    // visible to the runtime cleanup owner, even before a record exists.
    ensure_dir_synced(store.root(), &dir).context(LessonDeliveryWriteUnconfirmed)?;
    write_confirmed(store, skill, &path, &mut record, key)?;
    #[cfg(test)]
    super::publish::crash_hooks::check("lesson_delivery_unsent_durable")?;
    active(cancel, request.deadline)?;
    // This conservative state is confirmed BEFORE handoff permission returns.
    // A later crash or cancellation may still have sent nothing.
    record.payload.state = DeliveryState::PossibleHandoffUnknown;
    write_confirmed(store, skill, &path, &mut record, key)?;
    #[cfg(test)]
    super::publish::crash_hooks::check("lesson_delivery_possible_durable")?;
    active(cancel, request.deadline)?;
    Ok(())
}

fn active(cancel: &CancellationToken, deadline: Instant) -> Result<()> {
    ensure!(
        !cancel.is_cancelled() && Instant::now() < deadline,
        "lesson delivery was cancelled or expired"
    );
    Ok(())
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn delivery_id(key: &DecisionKey, owner: &OwnerScope, turn: &str, attempt: &str) -> String {
    hex::encode(key.hmac(
        KeyDomain::ReportOwnership,
        &[
            b"lesson-delivery-id-v1",
            owner.as_str().as_bytes(),
            turn.as_bytes(),
            attempt.as_bytes(),
        ],
    ))
}

fn existing_store_key(store: &SkillLifecycleStore) -> Result<std::sync::Arc<DecisionKey>> {
    let workspace = store
        .root()
        .parent()
        .and_then(Path::parent)
        .context("lifecycle workspace is unavailable")?;
    Ok(DecisionKey::load_existing(workspace)?)
}

fn write_confirmed(
    store: &SkillLifecycleStore,
    skill: &SkillId,
    path: &Path,
    record: &mut SealedDelivery,
    key: &DecisionKey,
) -> Result<()> {
    let payload = serde_json::to_vec(&record.payload)?;
    record.seal = hex::encode(key.hmac(KeyDomain::ReportOwnership, &[b"lesson-delivery-seal-v1", &payload]));
    let bytes = record_bytes(record, path)?;
    ensure!(
        u64::try_from(bytes.len())? <= MAX_RECORD_BYTES,
        "lesson delivery exceeds its record bound"
    );
    store
        .write_atomic(skill, path, &bytes)
        .context(LessonDeliveryWriteUnconfirmed)?;
    let actual = read_bytes(path, MAX_RECORD_BYTES).context(LessonDeliveryWriteUnconfirmed)?;
    ensure!(actual == bytes, LessonDeliveryWriteUnconfirmed);
    Ok(())
}

fn read_bytes(path: &Path, allowance: u64) -> Result<Vec<u8>> {
    check_private(path)?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    let bound = allowance.min(MAX_RECORD_BYTES);
    ensure!(
        metadata.is_file() && metadata.len() <= bound,
        "invalid lesson delivery record size or exhausted read allowance"
    );
    let mut bytes = Vec::new();
    (&mut file).take(bound).read_to_end(&mut bytes)?;
    ensure!(
        u64::try_from(bytes.len())? == metadata.len() && file.metadata()?.len() == metadata.len(),
        "lesson delivery record changed during bounded read"
    );
    Ok(bytes)
}

impl SkillLifecycleStore {
    /// The caller already owns this store's exclusive pruning lock. Do not
    /// recursively acquire shared store ownership or apply a new expiry rule.
    pub(super) fn delivery_retention_refs_locked(
        &self,
        _proof: &StoreLock,
        limits: DeliveryRetentionLimits,
    ) -> Result<DeliveryRetentionRefs> {
        ensure!(
            limits.max_records > 0 && limits.max_decoded_bytes > 0,
            "invalid delivery retention work bounds"
        );
        ensure!(
            Instant::now() < limits.deadline,
            "delivery retention work deadline exhausted"
        );
        let dir = self.root().join(DIRECTORY);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DeliveryRetentionRefs {
                    refs: Vec::new(),
                    examined_records: 0,
                    decoded_bytes: 0,
                });
            }
            Err(error) => return Err(error.into()),
        };
        check_private(&dir)?;
        let key = existing_store_key(self)?;
        ensure!(
            Instant::now() < limits.deadline,
            "delivery retention work deadline exhausted"
        );
        let mut examined_records = 0_usize;
        let mut decoded_bytes = 0_u64;
        let mut refs = BTreeMap::new();
        for entry in entries {
            ensure!(
                Instant::now() < limits.deadline,
                "delivery retention work deadline exhausted"
            );
            examined_records = examined_records
                .checked_add(1)
                .context("delivery retention count overflow")?;
            ensure!(
                examined_records <= limits.max_records,
                "delivery retention record bound exhausted"
            );
            let entry = entry?;
            let path = entry.path();
            let allowance = limits
                .max_decoded_bytes
                .checked_sub(decoded_bytes)
                .context("delivery retention byte bound exhausted")?;
            let bytes = read_bytes(&path, allowance)?;
            decoded_bytes = decoded_bytes
                .checked_add(u64::try_from(bytes.len())?)
                .context("delivery retention byte overflow")?;
            ensure!(
                decoded_bytes <= limits.max_decoded_bytes,
                "delivery retention byte bound exhausted"
            );
            let record: SealedDelivery = serde_json::from_slice(&bytes)?;
            let payload = &record.payload;
            let expected_name = format!("{}.json", payload.delivery_id);
            ensure!(
                payload.schema_version == 1
                    && payload.owner_scope == *self.owner_scope()
                    && entry.file_name() == std::ffi::OsStr::new(&expected_name)
                    && payload.delivery_id
                        == delivery_id(&key, self.owner_scope(), &payload.turn_id, &payload.attempt_id)
                    && !payload.refs.is_empty()
                    && payload.refs.len() <= 2
                    && [
                        payload.physical_binding_sha256.as_str(),
                        payload.snapshot_sha256.as_str(),
                        payload.block_sha256.as_str(),
                        payload.final_context_sha256.as_str(),
                        payload.body_binding_sha256.as_str(),
                        payload.experiment.experiment_sha256.as_str(),
                        payload.experiment.input_sha256.as_str(),
                    ]
                    .iter()
                    .all(|digest| valid_digest(digest)),
                "invalid owner-local lesson delivery reference"
            );
            let sealed = serde_json::to_vec(payload)?;
            let tag = hex::decode(&record.seal)?;
            ensure!(
                key.verify_hmac(KeyDomain::ReportOwnership, &[b"lesson-delivery-seal-v1", &sealed], &tag),
                "lesson delivery seal is invalid"
            );
            for reference in payload.refs.iter().cloned() {
                refs.insert(serde_json::to_vec(&reference)?, reference);
            }
            ensure!(
                Instant::now() < limits.deadline,
                "delivery retention work deadline exhausted"
            );
        }
        ensure!(
            Instant::now() < limits.deadline,
            "delivery retention work deadline exhausted"
        );
        Ok(DeliveryRetentionRefs {
            refs: refs.into_values().collect(),
            examined_records,
            decoded_bytes,
        })
    }
}
