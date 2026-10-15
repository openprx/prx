//! Publication journal: one durable record per publication or rollback
//! transaction, stored as `publication/<transaction-id>.json`.
//!
//! A transaction is written PREPARED before the active binding changes and
//! COMMITTED after it. The binding carries the transaction id, so a record left
//! PREPARED by a crash is resolved from the binding alone (see `recover`).
//!
//! Extension point for SL07/SL08 authorization: the gate/authorization receipt
//! that allowed the transition is to be added to [`JournalEntry`] as one more
//! pinned receipt hash field, written in the PREPARED record and re-verified
//! under the publication lock before the binding swap. It is intentionally not
//! present yet because no verifier exists to give it meaning.

use super::generation::ensure_dir_synced;
use super::store::{SkillLifecycleStore, check_private, read_record, record_bytes, sync_dir};
use super::types::{LifecycleError, OwnerScope, RevisionId, SCHEMA_VERSION, SkillId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::PathBuf;

pub(super) const PUBLICATION_DIR: &str = "publication";
const MAX_TRANSACTION_ID_BYTES: usize = 64;

/// Caller-chosen identity of a publication request; makes retries idempotent.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TransactionId(String);

impl TransactionId {
    /// Accepts 1..=64 ASCII letters, digits, `-` and `_`.
    pub fn parse(value: &str) -> Result<Self, LifecycleError> {
        let valid = !value.is_empty()
            && value.len() <= MAX_TRANSACTION_ID_BYTES
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if valid {
            Ok(Self(value.to_string()))
        } else {
            Err(LifecycleError::InvalidId {
                kind: "transaction id",
                reason: format!("must be 1..={MAX_TRANSACTION_ID_BYTES} ASCII letters, digits, '-' or '_'"),
            })
        }
    }

    #[must_use]
    pub fn generate() -> Self {
        Self(format!("tx-{}", uuid::Uuid::new_v4().simple()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TransactionId {
    type Error = LifecycleError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<TransactionId> for String {
    fn from(value: TransactionId) -> Self {
        value.0
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionKind {
    Publish,
    Rollback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionState {
    /// Written before the binding swap; unresolved after a crash.
    Prepared,
    /// The binding carried this transaction and the journal says so.
    Committed,
    /// The binding never carried this transaction; the candidate is not active.
    Aborted,
}

impl TransactionState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Aborted => "aborted",
        }
    }
}

/// One publication or rollback transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEntry {
    pub schema_version: u32,
    pub owner_scope: OwnerScope,
    pub transaction_id: TransactionId,
    pub kind: TransactionKind,
    pub skill_id: SkillId,
    /// Active revision and its pinned record digest when the transaction began.
    pub predecessor_revision_id: Option<RevisionId>,
    pub predecessor_record_digest: Option<String>,
    pub candidate_revision_id: RevisionId,
    /// Opaque `record_digest()` of the candidate; later reads pin to it.
    pub candidate_record_digest: String,
    pub expected_generation: u64,
    pub new_generation: u64,
    pub state: TransactionState,
    pub prepared_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl SkillLifecycleStore {
    fn journal_path(&self, transaction: &TransactionId) -> PathBuf {
        self.root()
            .join(PUBLICATION_DIR)
            .join(format!("{}.json", transaction.as_str()))
    }

    /// Reads one journal entry, `None` when the transaction is unknown.
    pub fn journal_entry(&self, transaction: &TransactionId) -> Result<Option<JournalEntry>, LifecycleError> {
        let path = self.journal_path(transaction);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(LifecycleError::io(&path, e)),
        }
        check_private(&path)?;
        let entry: JournalEntry = read_record(&path)?;
        let invalid = |reason: &str| LifecycleError::InvalidRecord {
            path: path.clone(),
            reason: reason.to_string(),
        };
        if entry.schema_version != SCHEMA_VERSION {
            return Err(LifecycleError::UnsupportedSchema {
                what: "journal",
                found: entry.schema_version,
            });
        }
        if entry.owner_scope != *self.owner_scope() {
            return Err(LifecycleError::OwnerMismatch {
                expected: self.owner_scope().to_string(),
                found: entry.owner_scope.to_string(),
            });
        }
        if entry.transaction_id != *transaction {
            return Err(invalid("transaction id does not match the file name"));
        }
        if entry.expected_generation.checked_add(1) != Some(entry.new_generation) {
            return Err(invalid("new generation is not expected generation + 1"));
        }
        Ok(Some(entry))
    }

    /// All journal entries, ordered by transaction id. A malformed `.json`
    /// entry fails the call: a damaged journal must be noticed. Dotfiles and
    /// files without the `.json` suffix are not journal entries and are ignored.
    pub fn journal_entries(&self) -> Result<Vec<JournalEntry>, LifecycleError> {
        self.scan_journal(None, true)
    }

    /// Entries of one skill. Entries that provably belong to another skill, or
    /// whose skill cannot be determined, are skipped even if malformed, so one
    /// damaged entry of another skill cannot block this skill. A malformed
    /// entry of this skill still fails the call.
    pub(super) fn journal_entries_for(&self, skill: &SkillId) -> Result<Vec<JournalEntry>, LifecycleError> {
        self.scan_journal(Some(skill), false)
    }

    /// Entries of all skills for recovery; malformed entries are skipped with
    /// a warning so they cannot block recovery of healthy skills.
    pub(super) fn journal_entries_lenient(&self) -> Result<Vec<JournalEntry>, LifecycleError> {
        self.scan_journal(None, false)
    }

    fn scan_journal(&self, skill: Option<&SkillId>, strict: bool) -> Result<Vec<JournalEntry>, LifecycleError> {
        let dir = self.root().join(PUBLICATION_DIR);
        let reader = match fs::read_dir(&dir) {
            Ok(reader) => reader,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(LifecycleError::io(&dir, e)),
        };
        let mut entries = Vec::new();
        for item in reader {
            let item = item.map_err(|e| LifecycleError::io(&dir, e))?;
            let name = item.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            let transaction = match TransactionId::parse(stem) {
                Ok(transaction) => transaction,
                Err(e) if strict => return Err(e),
                Err(e) => {
                    tracing::warn!(file = %name, error = %e, "ignoring journal file with an invalid name");
                    continue;
                }
            };
            if !strict && let Some(wanted) = skill {
                match self.entry_skill(&item.path()) {
                    Some(found) if found != wanted.as_str() => continue,
                    Some(_) => {}
                    None => {
                        tracing::warn!(file = %name, "ignoring journal file without a readable skill id");
                        continue;
                    }
                }
            }
            match self.journal_entry(&transaction) {
                Ok(Some(entry)) => entries.push(entry),
                Ok(None) => {}
                Err(e) if strict || skill.is_some() => return Err(e),
                Err(e) => tracing::warn!(file = %name, error = %e, "skipping malformed journal entry"),
            }
        }
        entries.sort_by(|a, b| a.transaction_id.cmp(&b.transaction_id));
        Ok(entries)
    }

    /// Skill id of a journal file without validating the rest of the entry.
    fn entry_skill(&self, path: &std::path::Path) -> Option<String> {
        let value: serde_json::Value = read_record(path).ok()?;
        value.get("skill_id")?.as_str().map(str::to_string)
    }

    /// Durably replaces the entry of its transaction (temp file, fsync,
    /// rename, directory fsync). For state changes of an existing entry.
    pub(super) fn write_journal(&self, entry: &JournalEntry) -> Result<(), LifecycleError> {
        let dir = self.root().join(PUBLICATION_DIR);
        ensure_dir_synced(self.root(), &dir)?;
        let path = self.journal_path(&entry.transaction_id);
        let bytes = record_bytes(entry, &path)?;
        self.write_atomic(&entry.skill_id, &path, &bytes)
    }

    /// Creates the entry of a new transaction with no-replace semantics: the
    /// file is fully written and fsynced, then hard-linked into place, so an
    /// entry another skill's transaction already owns is never overwritten
    /// (the per-skill lock does not cover the global journal path).
    pub(super) fn create_journal(&self, entry: &JournalEntry) -> Result<(), LifecycleError> {
        let dir = self.root().join(PUBLICATION_DIR);
        ensure_dir_synced(self.root(), &dir)?;
        let path = self.journal_path(&entry.transaction_id);
        let tmp = self.stage_file(&entry.skill_id, &record_bytes(entry, &path)?)?;
        let linked = fs::hard_link(&tmp, &path);
        if let Err(e) = fs::remove_file(&tmp) {
            tracing::warn!(path = %tmp.display(), error = %e, "failed to remove temporary journal file");
        }
        match linked {
            Ok(()) => sync_dir(&dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(LifecycleError::TransactionConflict {
                transaction_id: entry.transaction_id.to_string(),
            }),
            Err(e) => Err(LifecycleError::io(&path, e)),
        }
    }
}
