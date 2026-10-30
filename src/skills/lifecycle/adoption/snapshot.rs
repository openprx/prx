//! Frozen snapshot data is never adoption authority. Revalidate only bounded
//! signed origin pins under the caller's existing store lock; never replay a
//! judge/publication, repair history, select replacement guidance or send work.
use super::context::{LessonCompatibility, LessonSnapshot};
use super::lookup::LookupBudget;
use super::record::{INDEX_CAP, digest, read_bounded_bytes, refusal, valid_digest};
use crate::skills::lifecycle::{LifecycleError, RevisionId, SkillId, SkillLifecycleStore};
use chrono::{DateTime, Utc};

const FORMATTER: &str = "adopted-skill-ascii-v1";
const PROFILE_BOUNDARY_TOKENS: u64 = 320;

/// The complete literal block is deterministic from frozen data. Fresh elapsed
/// measurements never enter this rendering or its digest. An empty selection
/// creates no guidance and cannot imply a delivered lesson.
pub(in crate::skills::lifecycle) fn format_lesson_block(
    snapshot: &LessonSnapshot,
    stage: &str,
) -> Result<String, LifecycleError> {
    snapshot.limits.validate()?;
    crate::skills::lifecycle::TransactionId::parse(stage)?;
    if !stage.is_ascii()
        || snapshot.schema_version != 1
        || snapshot.formatter_version != FORMATTER
        || snapshot.entries.len() > usize::try_from(snapshot.limits.lessons).unwrap_or(usize::MAX)
    {
        return Err(refusal("unsupported frozen lesson formatter or stage"));
    }
    if snapshot.entries.is_empty() {
        return Ok(String::new());
    }
    let hash = digest(&serde_json::to_vec(snapshot).map_err(|_| refusal("lesson snapshot encoding failed"))?);
    let mut block = format!("BEGIN_PRX_LESSON {stage} {hash}\nUNTRUSTED DATA\n");
    for entry in &snapshot.entries {
        let text = &entry.sanitized_guidance;
        if !text.is_ascii()
            || text.is_empty()
            || text.chars().any(char::is_control)
            || text.contains("BEGIN_PRX_LESSON")
            || text.contains("END_PRX_LESSON")
            || u64::try_from(text.len()).unwrap_or(u64::MAX) > snapshot.limits.item_bytes
            || digest(text.as_bytes()) != entry.guidance_sha256
        {
            return Err(refusal("lesson guidance was changed, unsafe or oversized"));
        }
        block.push_str(text);
        block.push('\n');
    }
    block.push_str("END_PRX_LESSON\n");
    let bytes = u64::try_from(block.len()).map_err(|_| refusal("lesson block size overflow"))?;
    let upper = bytes
        .checked_add(PROFILE_BOUNDARY_TOKENS)
        .ok_or_else(|| refusal("lesson token upper bound overflow"))?;
    if bytes > snapshot.limits.context_bytes || upper > snapshot.limits.context_token_upper_bound {
        return Err(refusal("complete lesson block exceeds frozen byte/token allowance"));
    }
    Ok(block)
}

impl SkillLifecycleStore {
    /// Caller already holds the existing shared/exclusive store barrier. There
    /// is deliberately no nested store lock and no unbounded full-history replay.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::skills::lifecycle) fn validate_lesson_snapshot_locked(
        &self,
        snapshot: &LessonSnapshot,
        skill: &SkillId,
        base: &RevisionId,
        base_sha: &str,
        compatibility: &LessonCompatibility,
        roots: &[String],
        now: DateTime<Utc>,
    ) -> Result<(), LifecycleError> {
        let mut budget = LookupBudget::new(snapshot.limits)?;
        if snapshot.schema_version != 1
            || snapshot.owner_scope != *self.owner_scope()
            || snapshot.task_family.is_empty()
            || snapshot.task_family.len() > 128
            || snapshot.task_family.chars().any(char::is_control)
            || snapshot.formatter_version != FORMATTER
            || snapshot.compatibility != *compatibility
            || !valid_digest(base_sha)
            || !valid_digest(&snapshot.query_sha256)
            || !valid_digest(&snapshot.index_sha256)
            || snapshot.exclusions != roots
            || roots.iter().any(|root| !valid_digest(root))
            || roots.windows(2).any(|pair| pair.first() >= pair.get(1))
            || snapshot.planned_at > now
            || snapshot.expires_at <= now
            || snapshot.expires_at <= snapshot.planned_at
            || snapshot.entries.len() > usize::try_from(snapshot.limits.lessons).unwrap_or(usize::MAX)
            || snapshot.entries.windows(2).any(|pair| match pair {
                [first, second] => first.reference.adoption_id >= second.reference.adoption_id,
                _ => true,
            })
            || snapshot.stats.examined > snapshot.limits.examined
            || snapshot.stats.excluded > snapshot.stats.examined
            || snapshot.stats.index_bytes > snapshot.limits.index_bytes
            || snapshot.stats.decoded_bytes > snapshot.limits.decoded_bytes
            || snapshot.stats.elapsed_ms > snapshot.limits.elapsed_ms
        {
            return Err(refusal(
                "lesson snapshot differs from its actual owner/target/profile/window/bounds",
            ));
        }
        let index = self.read_adoption_index_bounded_locked(
            &snapshot.task_family,
            skill,
            snapshot.limits.index_bytes.min(INDEX_CAP),
        )?;
        budget.check()?;
        let (descriptors, index_hash, index_bytes) = if let Some((index, bytes)) = index {
            let length = u64::try_from(bytes.len()).map_err(|_| refusal("lesson index size overflow"))?;
            budget.stats.index_bytes = length;
            budget.charge(length)?;
            let hash = digest(&serde_json::to_vec(&index).map_err(|_| refusal("lesson index encoding failed"))?);
            (index.entries, hash, length)
        } else {
            (Vec::new(), digest(b"empty-adoption-index-v1"), 0)
        };
        if index_hash != snapshot.index_sha256 || index_bytes != snapshot.stats.index_bytes {
            return Err(refusal("frozen lesson index changed or its input accounting differs"));
        }
        let mut entries = snapshot.entries.iter().peekable();
        for descriptor in descriptors {
            let Some(expected) = entries.peek() else {
                break;
            };
            budget.examine()?; // Distractor descriptors count before limiting.
            match descriptor.reference.adoption_id.cmp(&expected.reference.adoption_id) {
                std::cmp::Ordering::Less => continue,
                std::cmp::Ordering::Greater => {
                    return Err(refusal("frozen lesson reference is absent from its signed partition"));
                }
                std::cmp::Ordering::Equal => {}
            }
            if descriptor.retired || descriptor.invalidated || descriptor.reference != expected.reference {
                return Err(refusal("lesson reference is retired, invalidated or replaced"));
            }
            let path = self.adoption_record_path(&descriptor.reference.adoption_id)?;
            budget.check()?;
            let bytes = read_bounded_bytes(&path, snapshot.limits.record_bytes)?;
            budget.charge(u64::try_from(bytes.len()).map_err(|_| refusal("lesson record size overflow"))?)?;
            let record = self.decode_adoption(&bytes, &descriptor.reference)?;
            budget.check()?;
            let barrier = read_bounded_bytes(
                &self.adoption_retention_barrier_path(&record)?,
                snapshot.limits.record_bytes,
            )?;
            budget.charge(u64::try_from(barrier.len()).unwrap_or(u64::MAX))?;
            self.check_adoption_retention_barrier(&record, &barrier)?;
            budget.check()?;
            if record.owner_scope != *self.owner_scope()
                || record.skill_id != *skill
                || record.task_family != snapshot.task_family
                || record.adopted_revision != *base
                || record.adopted_record_sha256 != base_sha
                || record.compatibility != *compatibility
                || record.expires_at <= now
                || expected.expires_at != record.expires_at
                || snapshot.expires_at > record.expires_at
                || record.excluded_roots.iter().any(|root| roots.contains(root))
                || expected.evidence_pin_sha256 != record.evidence_pin_sha256
                || !valid_digest(&expected.evidence_pin_sha256)
            {
                return Err(refusal(
                    "lesson signed source is incompatible, expired or overlaps current task roots",
                ));
            }
            let guidance = record.source.render()?;
            if guidance != expected.sanitized_guidance
                || digest(guidance.as_bytes()) != expected.guidance_sha256
                || u64::try_from(guidance.len()).unwrap_or(u64::MAX) > snapshot.limits.item_bytes
            {
                return Err(refusal(
                    "frozen guidance does not reconstruct from its signed adopted source",
                ));
            }
            // Preflight actual metadata before the existing bounded full-bundle
            // verifier reads content. A forged size cannot buy cheap decoding.
            let bundle = self.bundle_dir(skill, &record.adopted_revision);
            let actual_bytes = bounded_bundle_metadata(&bundle, record.adopted_bundle_bytes, &mut budget)?;
            if actual_bytes != record.adopted_bundle_bytes {
                return Err(refusal("adopted bundle size changed"));
            }
            budget.charge(actual_bytes)?;
            let revision_record = bundle
                .parent()
                .ok_or_else(|| refusal("adopted revision directory is missing"))?
                .join("revision.json");
            let metadata = std::fs::symlink_metadata(&revision_record)
                .map_err(|error| LifecycleError::io(&revision_record, error))?;
            if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > snapshot.limits.record_bytes
            {
                return Err(refusal("adopted revision record exceeds bounded immutable input"));
            }
            budget.charge(metadata.len())?;
            let revision = self.read_revision(skill, &record.adopted_revision, Some(&record.adopted_record_sha256))?;
            budget.check()?;
            let decoded = revision
                .bundle
                .files()
                .iter()
                .try_fold(0_u64, |sum, file| {
                    sum.checked_add(u64::try_from(file.bytes().len()).ok()?)
                })
                .ok_or_else(|| refusal("adopted source size overflow"))?;
            if decoded != actual_bytes {
                return Err(refusal("adopted immutable source changed while verifying"));
            }
            entries.next();
        }
        if entries.next().is_some() || snapshot.stats.examined < budget.stats.examined {
            return Err(refusal("lesson references or examined-work evidence are incomplete"));
        }
        // The factory separately renders with the actual admitted stage and
        // checks its complete block budget; do not substitute a guessed stage.
        budget.check()
    }
}

/// Exact known immutable bundle only. Every metadata hop checks the same small
/// file/depth/work ceilings before any asset body is decoded; no link is followed.
pub(super) fn bounded_bundle_metadata(
    root: &std::path::Path,
    expected: u64,
    budget: &mut LookupBudget,
) -> Result<u64, LifecycleError> {
    use crate::skills::lifecycle::bundle::{MAX_BUNDLE_DEPTH, MAX_BUNDLE_FILES};
    let mut pending = vec![(root.to_path_buf(), 0_usize)];
    let mut bytes = 0_u64;
    let mut examined = 0_usize;
    let mut files = 0_usize;
    while let Some((path, depth)) = pending.pop() {
        budget.check()?;
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| LifecycleError::io(&path, error))?;
        examined = examined
            .checked_add(1)
            .ok_or_else(|| refusal("lesson metadata work overflow"))?;
        if metadata.file_type().is_symlink()
            || depth > MAX_BUNDLE_DEPTH
            || examined > MAX_BUNDLE_FILES.saturating_mul(MAX_BUNDLE_DEPTH + 1)
        {
            return Err(refusal(
                "adopted source metadata is unsafe or exceeds its finite work bound",
            ));
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path).map_err(|error| LifecycleError::io(&path, error))? {
                budget.check()?;
                let entry = entry.map_err(|error| LifecycleError::io(&path, error))?;
                if pending.len() >= MAX_BUNDLE_FILES.saturating_mul(MAX_BUNDLE_DEPTH + 1) {
                    return Err(refusal("adopted source metadata fanout exceeds its finite bound"));
                }
                pending.push((entry.path(), depth + 1));
            }
        } else if metadata.is_file() {
            files += 1;
            bytes = bytes
                .checked_add(metadata.len())
                .ok_or_else(|| refusal("adopted source byte count overflow"))?;
            if files > MAX_BUNDLE_FILES || bytes > expected {
                return Err(refusal("adopted source exceeds its signed bounded size"));
            }
        } else {
            return Err(refusal("adopted source has an unsupported file type"));
        }
    }
    budget.check()?;
    Ok(bytes)
}
