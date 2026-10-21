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
use super::projection::ProjectionRepair;
use super::store::{STAGING_DIR, SkillLifecycleStore, sync_dir};
use super::types::{LifecycleError, RevisionId, SCHEMA_VERSION, SkillId};
use chrono::Utc;
use std::fs;
use std::time::{Duration, SystemTime};

/// Transaction id prefix of proposal publications (authorize.rs).
pub(super) const PROPOSAL_TX_PREFIX: &str = "pub-";
/// Transaction id prefix of legacy imports (migrate.rs).
pub(super) const MIGRATION_TX_PREFIX: &str = "mig-";

/// ASCII case-insensitive prefix test: `PUB-` must not slip past `pub-` and
/// land on the same file on a case-insensitive file system.
fn has_prefix(id: &str, prefix: &str) -> bool {
    id.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn refuse_reserved(transaction: &TransactionId) -> Result<(), LifecycleError> {
    let id = transaction.as_str();
    if has_prefix(id, PROPOSAL_TX_PREFIX) || has_prefix(id, MIGRATION_TX_PREFIX) {
        return Err(LifecycleError::ReservedTransactionId {
            transaction_id: id.to_string(),
        });
    }
    Ok(())
}

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
    /// Whether the skill is enabled once this revision is active. Journaled
    /// and part of the request fingerprint: retrying a transaction id with the
    /// other value is a `TransactionConflict`.
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
    /// Leftover entries removed from `staging/` and from the projection staging area.
    pub swept_staging: usize,
    /// Compatibility projections rewritten (or that could not be) because they
    /// did not match the committed binding.
    pub projections: ProjectionRepair,
}

/// Failure points for crash tests; compiled only under `cfg(test)`.
#[cfg(test)]
pub(super) mod crash_hooks {
    use super::LifecycleError;
    use std::cell::RefCell;

    thread_local! {
        static ARMED: RefCell<Option<&'static str>> = const { RefCell::new(None) };
        static ACTION: RefCell<Option<(&'static str, Box<dyn FnOnce()>)>> = const { RefCell::new(None) };
        static TWO_STEP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Forces the projection swap onto its two-rename fallback on this thread.
    pub(in crate::skills::lifecycle) fn force_two_step(on: bool) {
        TWO_STEP.with(|f| f.set(on));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(in crate::skills::lifecycle) fn two_step_forced() -> bool {
        TWO_STEP.with(std::cell::Cell::get)
    }

    /// Runs `action` once, on this thread, when execution reaches `point`
    /// (to move the world between two steps of a transition).
    pub(in crate::skills::lifecycle) fn run_at(point: &'static str, action: Box<dyn FnOnce()>) {
        ACTION.with(|a| *a.borrow_mut() = Some((point, action)));
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
        let due = ACTION.with(|a| {
            let mut slot = a.borrow_mut();
            if slot.as_ref().is_some_and(|(armed, _)| *armed == point) {
                slot.take().map(|(_, action)| action)
            } else {
                None
            }
        });
        if let Some(action) = due {
            action();
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
    /// `None` for rollbacks, whose enabled state comes from the target's anchor.
    enabled: Option<bool>,
    /// The authorization pin of the request; an entry without one is legacy.
    authorization_digest: Option<&'a str>,
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
            && self.enabled.is_none_or(|e| entry.enabled == e)
            // A pinned request resumes only an entry carrying the same pin; an
            // entry without one (legacy) is never taken for an authorized
            // publication, and an unpinned request never resumes a pinned one.
            && self.authorization_digest == entry.authorization_digest.as_deref()
    }
}

impl SkillLifecycleStore {
    /// Publishes `request.candidate` as the active revision if the active
    /// generation still equals `request.expected_generation`.
    ///
    /// This is the raw store-level transition: it applies no lifecycle mode,
    /// gate policy or authorization, so it is `pub(in crate::skills)`; code
    /// elsewhere goes through `SkillsLifecycleService` (test fixtures use
    /// `test_support`). The transaction id
    /// prefixes `pub-` (proposal publication) and `mig-` (legacy import) are
    /// reserved for their owners and refused here.
    ///
    /// Blocking: waits for locks and does file I/O; async callers must use
    /// `spawn_blocking`.
    pub(in crate::skills) fn publish(&self, request: &PublishRequest) -> Result<TransactionOutcome, LifecycleError> {
        refuse_reserved(&request.transaction_id)?;
        self.publish_inner(request, None)
    }

    /// Publication of an authorized proposal: the id must be `pub-...` and the
    /// authorization digest is pinned in the PREPARED entry. A retry of the
    /// same transaction id must carry the same digest; an entry without one,
    /// or with another, is a [`LifecycleError::TransactionConflict`].
    pub(super) fn publish_pinned(
        &self,
        request: &PublishRequest,
        authorization_digest: &str,
    ) -> Result<TransactionOutcome, LifecycleError> {
        if !request.transaction_id.as_str().starts_with(PROPOSAL_TX_PREFIX) {
            return Err(LifecycleError::ReservedTransactionId {
                transaction_id: request.transaction_id.to_string(),
            });
        }
        self.publish_inner(request, Some(authorization_digest))
    }

    /// Legacy import publication: the id must be `mig-...`.
    pub(super) fn publish_migration(&self, request: &PublishRequest) -> Result<TransactionOutcome, LifecycleError> {
        if !request.transaction_id.as_str().starts_with(MIGRATION_TX_PREFIX) {
            return Err(LifecycleError::ReservedTransactionId {
                transaction_id: request.transaction_id.to_string(),
            });
        }
        self.publish_inner(request, None)
    }

    fn publish_inner(
        &self,
        request: &PublishRequest,
        authorization_digest: Option<&str>,
    ) -> Result<TransactionOutcome, LifecycleError> {
        self.ensure_publication_dirs()?;
        let _store_lock = self.lock_store(true, PUBLICATION_LOCK_TIMEOUT)?;
        let lock = self.lock_skill(&request.skill_id, PUBLICATION_LOCK_TIMEOUT)?;
        let fingerprint = Fingerprint {
            kind: TransactionKind::Publish,
            skill: &request.skill_id,
            candidate: &request.candidate,
            digest: Some(&request.candidate_record_digest),
            expected_generation: request.expected_generation,
            enabled: Some(request.enabled),
            authorization_digest,
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
            authorization_digest,
        )
    }

    /// Makes a revision that was committed earlier the active one again, under
    /// a new generation, with the enabled state its committing transaction had. History is not rewritten and no revision is deleted,
    /// so tasks pinned to any revision keep reading it. The pinned digest comes
    /// from the journal entry that committed the target, not from the caller.
    ///
    /// Blocking, like [`Self::publish`].
    pub(in crate::skills) fn rollback(&self, request: &RollbackRequest) -> Result<TransactionOutcome, LifecycleError> {
        self.ensure_publication_dirs()?;
        let _store_lock = self.lock_store(true, PUBLICATION_LOCK_TIMEOUT)?;
        let lock = self.lock_skill(&request.skill_id, PUBLICATION_LOCK_TIMEOUT)?;
        let fingerprint = Fingerprint {
            kind: TransactionKind::Rollback,
            skill: &request.skill_id,
            candidate: &request.target,
            digest: None,
            expected_generation: request.expected_generation,
            enabled: None,
            authorization_digest: None,
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
            anchor.enabled,
            None,
        )
    }

    /// The COMMITTED journal entry of the publication `transaction` of
    /// `skill` when it is live, `None` when it is not. A PREPARED entry of a
    /// dead writer is first reconciled by the rule recovery uses. `None` for an
    /// ABORTED entry and for no entry. A binding that carries the transaction
    /// while its entry is not committed is a tampered store. Read by the
    /// operator repair path before it closes a proposal; takes the store and
    /// skill locks.
    pub(super) fn live_publication(
        &self,
        skill: &SkillId,
        transaction: &TransactionId,
    ) -> Result<Option<JournalEntry>, LifecycleError> {
        self.ensure_publication_dirs()?;
        let _store_lock = self.lock_store(true, PUBLICATION_LOCK_TIMEOUT)?;
        let lock = self.lock_skill(skill, PUBLICATION_LOCK_TIMEOUT)?;
        let Some(mut entry) = self.journal_entry(transaction)? else {
            return Ok(None);
        };
        if entry.state == TransactionState::Prepared {
            self.reconcile(&lock, &mut entry)?;
        }
        if entry.state == TransactionState::Committed {
            return Ok(Some(entry));
        }
        if self
            .active_binding(skill)?
            .is_some_and(|binding| &binding.transaction_id == transaction)
        {
            return Err(LifecycleError::InvalidRecord {
                path: self.root().to_path_buf(),
                reason: format!("the binding of {skill} carries {transaction} but its journal entry is not committed"),
            });
        }
        Ok(None)
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
            TransactionState::Committed => {
                // A retry of a committed transaction completes a projection
                // that an earlier attempt could not finish.
                if let Some(binding) = self
                    .active_binding(&entry.skill_id)?
                    .filter(|b| b.transaction_id == entry.transaction_id)
                    && let Err(source) = self.refresh_projection(lock, &binding)
                {
                    return Err(LifecycleError::CommittedRepairRequired {
                        transaction_id: entry.transaction_id.to_string(),
                        reason: source.to_string(),
                    });
                }
                Ok(Some(TransactionOutcome { entry, replayed: true }))
            }
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
        authorization_digest: Option<&str>,
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
        // Refuse before anything is journaled when the projection cannot be
        // written (unsafe or unmanaged target, too many skill directories).
        self.precheck_projection(skill, candidate, predecessor.is_some())?;

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
            enabled,
            authorization_digest: authorization_digest.map(str::to_string),
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
        self.write_journal(entry)?;
        crash_point!("after_committed");
        // Spec 8.3 steps 7-8: the projection and the caches follow the commit.
        // A failure here leaves the binding live, so the caller sees
        // `CommittedRepairRequired` (see `classify_failure`).
        self.refresh_projection(lock, binding).map(|_| ())
    }

    /// An error after the swap may have started is not a plain failure: when
    /// the binding carries the transaction, the mutation is live and the
    /// caller must be told so (spec 8.3 step 8).
    fn classify_failure(&self, skill: &SkillId, entry: &JournalEntry, source: LifecycleError) -> LifecycleError {
        // The rename may be visible but not yet durable; read the binding only
        // after the directory was flushed.
        if let Err(e) = sync_dir(&self.root().join(super::generation::ACTIVE_DIR)) {
            tracing::warn!(error = %e, "failed to sync the active binding directory before classifying a failure");
        }
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
        // Repair first: the sweep keeps the previous tree of a skill whose
        // repair failed, so a failed repair never deletes its only copy.
        report.projections = self.repair_projections(lock_timeout);
        report.swept_staging = self
            .sweep_staging(staging_min_age)?
            .saturating_add(self.sweep_projection_staging(staging_min_age, &report.projections.keep));
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
    SkillId::parse(skill).ok().or_else(|| SkillId::reserved(skill))
}
