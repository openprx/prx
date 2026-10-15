//! Persistent record types and typed errors of the skill lifecycle store.

use super::bundle::BundleError;
use super::manifest::ManifestError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use thiserror::Error;

/// Schema version written into every persistent lifecycle record.
pub const SCHEMA_VERSION: u32 = 1;

const MAX_OWNER_SCOPE_BYTES: usize = 512;
const MAX_SKILL_ID_BYTES: usize = 64;
const REVISION_ID_PREFIX: &str = "r1-";
const REVISION_DIGEST_HEX_LEN: usize = 64;

/// Failure of a lifecycle operation. Every variant is a refusal; nothing is
/// downgraded to a warning.
#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("invalid {kind}: {reason}")]
    InvalidId { kind: &'static str, reason: String },
    #[error("invalid owner scope: {0}")]
    InvalidOwnerScope(String),
    #[error("bundle rejected: {0}")]
    Bundle(#[from] BundleError),
    #[error("manifest rejected: {0}")]
    Manifest(#[from] ManifestError),
    #[error("workspace {path} is not usable as an owner workspace: {reason}")]
    WorkspaceUnavailable { path: PathBuf, reason: String },
    #[error("unsafe store path {path}: {reason}")]
    UnsafeStorePath { path: PathBuf, reason: String },
    #[error("{path} has mode {mode:o}; owner-only permissions are required")]
    PermissionsTooOpen { path: PathBuf, mode: u32 },
    #[error("record belongs to owner scope {found:?}, expected {expected:?}")]
    OwnerMismatch { expected: String, found: String },
    #[error("unsupported {what} schema version {found}")]
    UnsupportedSchema { what: &'static str, found: u32 },
    #[error("revision {revision} already exists with different {field}")]
    RevisionConflict { revision: String, field: &'static str },
    #[error("tampered or inconsistent revision at {path}: {reason}")]
    Tampered { path: PathBuf, reason: String },
    #[error("revision not found: {0}")]
    RevisionNotFound(String),
    #[error("record {path} is invalid: {reason}")]
    InvalidRecord { path: PathBuf, reason: String },
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl LifecycleError {
    pub(super) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Owner scope of a record: the `owner_id` the runtime attached to the task
/// (the value `tools::sessions_read_model::effective_run_owner` returns, i.e.
/// `OwnerPrincipal::owner_id`). It is validated, never composed from model
/// supplied text.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OwnerScope(String);

impl OwnerScope {
    pub fn new(owner_id: &str) -> Result<Self, LifecycleError> {
        let trimmed = owner_id.trim();
        if trimmed.is_empty() {
            return Err(LifecycleError::InvalidOwnerScope("owner id is empty".to_string()));
        }
        if trimmed != owner_id {
            return Err(LifecycleError::InvalidOwnerScope(
                "owner id has surrounding whitespace".to_string(),
            ));
        }
        if owner_id.len() > MAX_OWNER_SCOPE_BYTES {
            return Err(LifecycleError::InvalidOwnerScope(format!(
                "owner id exceeds {MAX_OWNER_SCOPE_BYTES} bytes"
            )));
        }
        if owner_id.chars().any(char::is_control) {
            return Err(LifecycleError::InvalidOwnerScope(
                "owner id contains control characters".to_string(),
            ));
        }
        Ok(Self(owner_id.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for OwnerScope {
    type Error = LifecycleError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<OwnerScope> for String {
    fn from(value: OwnerScope) -> Self {
        value.0
    }
}

impl fmt::Display for OwnerScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable identity of a skill inside one owner scope. It is independent of the
/// display name and is a single path-safe segment: `[a-z0-9][a-z0-9_-]{0,63}`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SkillId(String);

impl SkillId {
    pub fn parse(value: &str) -> Result<Self, LifecycleError> {
        let invalid = |reason: &str| LifecycleError::InvalidId {
            kind: "skill id",
            reason: reason.to_string(),
        };
        if value.is_empty() || value.len() > MAX_SKILL_ID_BYTES {
            return Err(invalid("length must be 1..=64 bytes"));
        }
        let mut chars = value.chars();
        let first_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        if !first_ok || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-') {
            return Err(invalid("must match [a-z0-9][a-z0-9_-]*"));
        }
        Ok(Self(value.to_string()))
    }

    /// Allocates a fresh identity that carries no display-name information.
    #[must_use]
    pub fn generate() -> Self {
        Self(format!("skill-{}", uuid::Uuid::new_v4().simple()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SkillId {
    type Error = LifecycleError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<SkillId> for String {
    fn from(value: SkillId) -> Self {
        value.0
    }
}

impl fmt::Display for SkillId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identity of one complete canonical bundle: `r1-` followed by the lowercase
/// hex SHA-256 of the version-1 canonical bundle encoding.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RevisionId(String);

impl RevisionId {
    pub fn parse(value: &str) -> Result<Self, LifecycleError> {
        let invalid = |reason: &str| LifecycleError::InvalidId {
            kind: "revision id",
            reason: reason.to_string(),
        };
        let digest = value
            .strip_prefix(REVISION_ID_PREFIX)
            .ok_or_else(|| invalid("missing r1- prefix"))?;
        if digest.len() != REVISION_DIGEST_HEX_LEN
            || !digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("digest must be 64 lowercase hex characters"));
        }
        Ok(Self(value.to_string()))
    }

    pub(super) fn from_digest(digest: &[u8; 32]) -> Self {
        Self(format!("{REVISION_ID_PREFIX}{}", hex::encode(digest)))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The bare hex digest (the bundle manifest hash).
    #[must_use]
    pub fn digest_hex(&self) -> &str {
        self.0.strip_prefix(REVISION_ID_PREFIX).unwrap_or(&self.0)
    }
}

impl TryFrom<String> for RevisionId {
    type Error = LifecycleError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<RevisionId> for String {
    fn from(value: RevisionId) -> Self {
        value.0
    }
}

impl fmt::Display for RevisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a revision came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Operator,
    Learned,
    Upstream,
    Fork,
}

/// Immutable revision record stored next to its bundle as `revision.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillRevision {
    pub schema_version: u32,
    /// Stable record id; for a revision it equals the revision id.
    pub record_id: String,
    pub owner_scope: OwnerScope,
    pub skill_id: SkillId,
    pub revision_id: RevisionId,
    pub parent_revision_id: Option<RevisionId>,
    pub display_name: String,
    pub description: String,
    pub semantic_version: Option<String>,
    /// Hex SHA-256 of the canonical bundle encoding (the revision digest).
    pub bundle_manifest_hash: String,
    pub bundle_format_version: u32,
    pub source_kind: SourceKind,
    pub source_commit: Option<String>,
    pub proposal_id: Option<String>,
    pub pattern_ids: Vec<String>,
    pub capability_requirements: Vec<String>,
    pub review_receipt_ids: Vec<String>,
    pub validation_receipt_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    /// Actual unix mode (permission bits) of the revision directory when it
    /// was published; `None` where the platform has no mode bits.
    pub recorded_dir_mode: Option<u32>,
}

/// Caller supplied provenance for a new revision. Display name, description
/// and semantic version are derived from the validated bundle manifest.
#[derive(Clone, Debug)]
pub struct NewRevision {
    pub skill_id: SkillId,
    pub parent_revision_id: Option<RevisionId>,
    pub source_kind: SourceKind,
    pub source_commit: Option<String>,
    pub proposal_id: Option<String>,
    pub pattern_ids: Vec<String>,
    pub capability_requirements: Vec<String>,
    pub review_receipt_ids: Vec<String>,
    pub validation_receipt_ids: Vec<String>,
}

impl NewRevision {
    #[must_use]
    pub const fn new(skill_id: SkillId, source_kind: SourceKind) -> Self {
        Self {
            skill_id,
            parent_revision_id: None,
            source_kind,
            source_commit: None,
            proposal_id: None,
            pattern_ids: Vec::new(),
            capability_requirements: Vec::new(),
            review_receipt_ids: Vec::new(),
            validation_receipt_ids: Vec::new(),
        }
    }
}
