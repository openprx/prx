//! Publication, rollback and crash recovery (SL08 store level).
//!
//! Every transition runs under the skill's publication lock:
//! lock, idempotency check, generation check, candidate verification pinned to
//! its record digest, durable PREPARED journal entry, generation CAS of the
//! active binding, COMMITTED journal entry. If the process dies or an error
//! occurs after PREPARED, the binding alone decides the outcome in
//! [`SkillLifecycleStore::recover`]: a binding carrying the transaction id means
//! committed, anything else means the candidate was never published. A
//! recovered or aborted transaction is never re-run: a new transaction id (and,
//! at the SL07 binding, a fresh authorization) is required.

use super::generation::{ATOMIC_PREFIX, ATOMIC_SUFFIX, ActiveBinding, SkillLock};
use super::journal::{JournalEntry, TransactionId, TransactionKind, TransactionState};
use super::store::{STAGING_DIR, SkillLifecycleStore, sync_dir};
use super::types::{LifecycleError, RevisionId, SCHEMA_VERSION, SkillId};
use chrono::Utc;
use std::fs;
use std::time::{Duration, SystemTime};

/// How long a transition waits for the skill's publication lock.
pub const PUBLICATION_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Request to make an already written revision the active one.
#[derive(Clone, Debug)]
pub struct PublishRequest {
    pub transaction_id: TransactionId,
    pub skill_id: SkillId,
    pub candidate: RevisionId,
    /// `record_digest` returned when the revision was written; pinned in the journal.
    pub candidate_record_digest: String,
    /// Generation the caller observed; 0 when nothing was published yet.
    pub expected_generation: u64,
    /// Whether the skill is enabled once this revision is active. Not part of
    /// the journaled fingerprint: a caller that retries a transaction id with
    /// another value gets the original outcome, so choose ids accordingly.
    pub enabled: bool,
}

/// Request to re-activate a previously committed revision under a new generation.
#[derive(Clone, Debug)]
pub struct RollbackRequest {
    pub transaction_id: TransactionId,
    pub skill_id: SkillId,
    pub target: RevisionId,
    pub expected_generation: u64,
}

/// Result of a committed transition.
#[derive(Clone, Debug)]
pub struct TransactionOutcome {
    pub entry: JournalEntry,
    /// `true` when the transaction had already committed (idempotent retry,
    /// possibly completed by recovery) and nothing new was written.
    pub replayed: bool,
}

/// What [`SkillLifecycleStore::recover`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// PREPARED entries whose binding swap had happened; now COMMITTED.
    pub committed: Vec<TransactionId>,
    /// PREPARED entries whose binding swap never happened; now ABORTED.
    pub aborted: Vec<TransactionId>,
    /// Leftover entries removed from `staging/`.
    pub swept_staging: usize,
}

/// Failure points for crash tests; compiled only under `cfg(test)`.
#[cfg(test)]
pub(super) mod crash_hooks {
    use super::LifecycleError;
    use std::cell::RefCell;

    thread_local! {
        static ARMED: RefCell<Option<&'static str>> = const { RefCell::new(None) };
    }

    pub(in crate::skills::lifecycle) fn arm(point: &'static str) {
        ARMED.with(|a| *a.borrow_mut() = Some(point));
    }

    pub(in crate::skills::lifecycle) fn disarm() {
        ARMED.with(|a| *a.borrow_mut() = None);
    }

    /// Simulated process death: returns an error and leaves all state as is.
    pub(in crate::skills::lifecycle) fn check(point: &str) -> Result<(), LifecycleError> {
        // Widens the window between the generation check and the swap so
        // process-race tests overlap deterministically.
        if point == "binding_staged"
            && let Some(ms) = std::env::var("PRX_SL08_HOLD_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
        {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        let hit = ARMED.with(|a| a.borrow().is_some_and(|armed| armed == point));
        if hit {
            disarm();
            return Err(LifecycleError::io(point, std::io::Error::other("injected crash point")));
        }
        Ok(())
    }
}

struct Fingerprint<'a> {
    kind: TransactionKind,
    skill: &'a SkillId,
    candidate: &'a RevisionId,
    digest: Option<&'a str>,
    expected_generation: u64,
}

impl Fingerprint<'_> {
    fn matches(&self, entry: &JournalEntry) -> bool {
        (
            entry.kind,
            &entry.skill_id,
            &entry.candidate_revision_id,
            entry.expected_generation,
        ) == (self.kind, self.skill, self.candidate, self.expected_generation)
            && self.digest.is_none_or(|d| entry.candidate_record_digest == d)
    }
}

impl SkillLifecycleStore {
    /// Publishes `request.candidate` as the active revision if the active
    /// generation still equals `request.expected_generation`.
    ///
    /// Blocking: waits for locks and does file I/O; async callers must use
    /// `spawn_blocking`.
    pub fn publish(&self, request: &PublishRequest) -> Result<TransactionOutcome, LifecycleError> {
        self.ensure_publication_dirs()?;
        let _store_lock = self.lock_store(true, PUBLICATION_LOCK_TIMEOUT)?;
        let lock = self.lock_skill(&request.skill_id, PUBLICATION_LOCK_TIMEOUT)?;
        let fingerprint = Fingerprint {
            kind: TransactionKind::Publish,
            skill: &request.skill_id,
            candidate: &request.candidate,
            digest: Some(&request.candidate_record_digest),
            expected_generation: request.expected_generation,
        };
        if let Some(outcome) = self.resolve_existing(&lock, &request.transaction_id, &fingerprint)? {
            return Ok(outcome);
        }
        self.run_transition(
            &lock,
            &request.transaction_id,
            TransactionKind::Publish,
            &request.candidate,
            &request.candidate_record_digest,
            request.expected_generation,
            request.enabled,
        )
    }

    /// Makes a revision that was committed earlier the active one again, under
    /// a new generation. History is not rewritten and no revision is deleted,
    /// so tasks pinned to any revision keep reading it. The pinned digest comes
    /// from the journal entry that committed the target, not from the caller.
    ///
    /// Blocking, like [`Self::publish`].
    pub fn rollback(&self, request: &RollbackRequest) -> Result<TransactionOutcome, LifecycleError> {
        self.ensure_publication_dirs()?;
        let _store_lock = self.lock_store(true, PUBLICATION_LOCK_TIMEOUT)?;
        let lock = self.lock_skill(&request.skill_id, PUBLICATION_LOCK_TIMEOUT)?;
        let fingerprint = Fingerprint {
            kind: TransactionKind::Rollback,
            skill: &request.skill_id,
            candidate: &request.target,
            digest: None,
            expected_generation: request.expected_generation,
        };
        if let Some(outcome) = self.resolve_existing(&lock, &request.transaction_id, &fingerprint)? {
            return Ok(outcome);
        }
        let anchor = self
            .journal_entries_for(&request.skill_id)?
            .into_iter()
            .filter(|e| {
                (&e.skill_id, &e.candidate_revision_id, e.state)
                    == (&request.skill_id, &request.target, TransactionState::Committed)
            })
            .max_by_key(|e| e.new_generation)
            .ok_or_else(|| LifecycleError::RollbackTargetUnknown(format!("{}/{}", request.skill_id, request.target)))?;
        self.run_transition(
            &lock,
            &request.transaction_id,
            TransactionKind::Rollback,
            &request.target,
            &anchor.candidate_record_digest,
            request.expected_generation,
            true,
        )
    }

    /// Duplicate handling for a transaction id seen before. `None` means the
    /// id is new. Must run under the skill lock, so a PREPARED entry found
    /// here belongs to a dead writer and is reconciled first.
    fn resolve_existing(
        &self,
        lock: &SkillLock,
        transaction: &TransactionId,
        fingerprint: &Fingerprint<'_>,
    ) -> Result<Option<TransactionOutcome>, LifecycleError> {
        let Some(mut entry) = self.journal_entry(transaction)? else {
            return Ok(None);
        };
        if !fingerprint.matches(&entry) {
            return Err(LifecycleError::TransactionConflict {
                transaction_id: transaction.to_string(),
            });
        }
        if entry.state == TransactionState::Prepared {
            self.reconcile(lock, &mut entry)?;
        }
        match entry.state {
            TransactionState::Committed => Ok(Some(TransactionOutcome { entry, replayed: true })),
            state => Err(LifecycleError::TransactionNotCommitted {
                transaction_id: transaction.to_string(),
                state: state.as_str(),
            }),
        }
    }

    fn run_transition(
        &self,
        lock: &SkillLock,
        transaction: &TransactionId,
        kind: TransactionKind,
        candidate: &RevisionId,
        candidate_digest: &str,
        expected_generation: u64,
        enabled: bool,
    ) -> Result<TransactionOutcome, LifecycleError> {
        let skill = lock.skill();
        // A PREPARED entry of this skill whose swap went live must be settled
        // before anything else changes the binding; afterwards the binding no
        // longer shows it.
        self.reconcile_skill(lock)?;
        let predecessor = self.active_binding(skill)?;
        let actual = predecessor.as_ref().map_or(0, |b| b.generation);
        if actual != expected_generation {
            return Err(LifecycleError::StaleGeneration {
                skill: skill.to_string(),
                expected: expected_generation,
                actual,
            });
        }
        if predecessor.as_ref().is_some_and(|b| b.revision_id == *candidate) {
            return Err(LifecycleError::RevisionAlreadyActive(format!("{skill}/{candidate}")));
        }
        let new_generation = expected_generation
            .checked_add(1)
            .ok_or_else(|| LifecycleError::InvalidRecord {
                path: self.root().to_path_buf(),
                reason: "generation counter exhausted".to_string(),
            })?;
        // Verifies record, owner, manifest and the full bundle hash, and that
        // the record still has the digest pinned at write time.
        self.read_revision(skill, candidate, Some(candidate_digest))?;

        let mut entry = JournalEntry {
            schema_version: SCHEMA_VERSION,
            owner_scope: self.owner_scope().clone(),
            transaction_id: transaction.clone(),
            kind,
            skill_id: skill.clone(),
            predecessor_revision_id: predecessor.as_ref().map(|b| b.revision_id.clone()),
            predecessor_record_digest: predecessor.as_ref().map(|b| b.record_digest.clone()),
            candidate_revision_id: candidate.clone(),
            candidate_record_digest: candidate_digest.to_string(),
            expected_generation,
            new_generation,
            state: TransactionState::Prepared,
            prepared_at: Utc::now(),
            finished_at: None,
        };
        self.create_journal(&entry)?;
        crash_point!("after_prepared");

        let binding = ActiveBinding {
            schema_version: SCHEMA_VERSION,
            owner_scope: self.owner_scope().clone(),
            skill_id: skill.clone(),
            revision_id: candidate.clone(),
            record_digest: candidate_digest.to_string(),
            generation: new_generation,
            enabled,
            transaction_id: transaction.clone(),
            updated_at: Utc::now(),
        };
        // From here on the entry stays PREPARED when anything fails, on
        // purpose: whether the swap happened is decided from the binding.
        if let Err(source) = self.finish_transition(lock, &binding, &mut entry, expected_generation) {
            return Err(self.classify_failure(skill, &entry, source));
        }
        Ok(TransactionOutcome { entry, replayed: false })
    }

    fn finish_transition(
        &self,
        lock: &SkillLock,
        binding: &ActiveBinding,
        entry: &mut JournalEntry,
        expected_generation: u64,
    ) -> Result<(), LifecycleError> {
        self.cas_binding(lock, binding, expected_generation)?;
        crash_point!("after_binding");
        entry.state = TransactionState::Committed;
        entry.finished_at = Some(Utc::now());
        self.write_journal(entry)
    }

    /// An error after the swap may have started is not a plain failure: when
    /// the binding carries the transaction, the mutation is live and the
    /// caller must be told so (spec 8.3 step 8).
    fn classify_failure(&self, skill: &SkillId, entry: &JournalEntry, source: LifecycleError) -> LifecycleError {
        match self.active_binding(skill) {
            Ok(Some(b)) if (&b.transaction_id, b.generation) == (&entry.transaction_id, entry.new_generation) => {
                LifecycleError::CommittedRepairRequired {
                    transaction_id: entry.transaction_id.to_string(),
                    reason: source.to_string(),
                }
            }
            _ => source,
        }
    }

    /// Settles every PREPARED entry of the locked skill.
    fn reconcile_skill(&self, lock: &SkillLock) -> Result<(), LifecycleError> {
        for mut entry in self.journal_entries_for(lock.skill())? {
            if entry.state == TransactionState::Prepared {
                self.reconcile(lock, &mut entry)?;
            }
        }
        Ok(())
    }

    /// Resolves a PREPARED entry. Caller holds the skill lock. The entry was
    /// live if the binding carries it, or if a later transaction of the same
    /// skill recorded it as its predecessor at the generation it created (a
    /// later transaction can only start from the live binding).
    fn reconcile(&self, lock: &SkillLock, entry: &mut JournalEntry) -> Result<(), LifecycleError> {
        if lock.skill() != &entry.skill_id {
            return Err(LifecycleError::UnsafeStorePath {
                path: self.root().to_path_buf(),
                reason: "publication lock belongs to a different skill".to_string(),
            });
        }
        sync_dir(&self.root().join(super::generation::ACTIVE_DIR))?;
        let binding_live = self.active_binding(&entry.skill_id)?.is_some_and(|b| {
            (&b.transaction_id, b.generation, &b.revision_id, &b.record_digest)
                == (
                    &entry.transaction_id,
                    entry.new_generation,
                    &entry.candidate_revision_id,
                    &entry.candidate_record_digest,
                )
        });
        let swapped = binding_live
            || self.journal_entries_for(&entry.skill_id)?.iter().any(|later| {
                later.transaction_id != entry.transaction_id
                    && (later.expected_generation, later.predecessor_revision_id.as_ref())
                        == (entry.new_generation, Some(&entry.candidate_revision_id))
            });
        entry.state = if swapped {
            TransactionState::Committed
        } else {
            TransactionState::Aborted
        };
        entry.finished_at = Some(Utc::now());
        self.write_journal(entry)
    }

    /// Crash recovery: resolves every PREPARED journal entry (committed when
    /// the binding or a later transaction shows it was live, aborted
    /// otherwise; an aborted candidate is never published) and sweeps
    /// `staging/` leftovers older than `staging_min_age`.
    ///
    /// `Duration::ZERO` means exclusive startup recovery: it takes the
    /// store-wide lock exclusively, so no publication or revision write is in
    /// flight, and fails with `LockTimeout` otherwise. A non-zero age is safe
    /// next to running writers: entries are renamed to a trash name before
    /// deletion, and files staged for a skill are only removed when that
    /// skill's lock can be taken.
    ///
    /// Blocking; async callers must use `spawn_blocking`.
    pub fn recover(&self, staging_min_age: Duration) -> Result<RecoveryReport, LifecycleError> {
        self.recover_with_timeout(staging_min_age, PUBLICATION_LOCK_TIMEOUT)
    }

    pub(super) fn recover_with_timeout(
        &self,
        staging_min_age: Duration,
        lock_timeout: Duration,
    ) -> Result<RecoveryReport, LifecycleError> {
        self.ensure_publication_dirs()?;
        let _store_lock = self.lock_store(!staging_min_age.is_zero(), lock_timeout)?;
        let mut report = RecoveryReport::default();
        for listed in self.journal_entries_lenient()? {
            if listed.state != TransactionState::Prepared {
                continue;
            }
            let lock = self.lock_skill(&listed.skill_id, lock_timeout)?;
            // Re-read under the lock; another process may have finished it.
            let Some(mut entry) = self.journal_entry(&listed.transaction_id)? else {
                continue;
            };
            if entry.state != TransactionState::Prepared {
                continue;
            }
            self.reconcile(&lock, &mut entry)?;
            match entry.state {
                TransactionState::Committed => report.committed.push(entry.transaction_id),
                _ => report.aborted.push(entry.transaction_id),
            }
        }
        report.swept_staging = self.sweep_staging(staging_min_age)?;
        Ok(report)
    }

    fn sweep_staging(&self, min_age: Duration) -> Result<usize, LifecycleError> {
        let dir = self.root().join(STAGING_DIR);
        let mut removed = 0;
        for item in fs::read_dir(&dir).map_err(|e| LifecycleError::io(&dir, e))? {
            let item = item.map_err(|e| LifecycleError::io(&dir, e))?;
            let path = item.path();
            let name = item.file_name().to_string_lossy().into_owned();
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(LifecycleError::io(&path, e)),
            };
            let age = meta
                .modified()
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .unwrap_or_default();
            if age < min_age {
                continue;
            }
            // A staged binding or journal file belongs to a skill; it may only
            // go while that skill's lock is free (the lock holder may be
            // between staging and renaming it).
            let _skill_lock = match staged_file_skill(&name) {
                Some(skill) => match self.lock_skill(&skill, Duration::ZERO) {
                    Ok(lock) => Some(lock),
                    Err(LifecycleError::LockTimeout { .. }) => continue,
                    Err(e) => return Err(e),
                },
                None => None,
            };
            // Rename first: a writer that renames the same entry into place
            // concurrently either wins (our rename sees NotFound) or loses its
            // own rename; we never delete files inside a path that can still
            // become live.
            let trash = dir.join(format!(".trash-{}", uuid::Uuid::new_v4()));
            if !name.starts_with(".trash-") {
                match fs::rename(&path, &trash) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "failed to move skill staging entry to trash");
                        continue;
                    }
                }
            }
            let victim = if name.starts_with(".trash-") { &path } else { &trash };
            let result = if meta.is_dir() {
                fs::remove_dir_all(victim)
            } else {
                fs::remove_file(victim)
            };
            match result {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(path = %victim.display(), error = %e, "failed to sweep skill staging entry"),
            }
        }
        Ok(removed)
    }
}

/// Skill of a staged `atomic-<skill>-<uuid>.tmp` file, `None` for other names.
fn staged_file_skill(name: &str) -> Option<SkillId> {
    const UUID_LEN: usize = 36;
    let middle = name.strip_prefix(ATOMIC_PREFIX)?.strip_suffix(ATOMIC_SUFFIX)?;
    let skill = middle.get(..middle.len().checked_sub(UUID_LEN + 1)?)?;
    SkillId::parse(skill).ok()
}
