//! The causal decision store (SP-CDP-A 5.10, CDP-PLAN 1.8, CDP 14).
//!
//! Decisions and outcomes are operational telemetry, not agent memory: the
//! store is `<workspace>/self/causal_tree/decisions.db`, apart from the memory
//! database and every memory category. It is non-authoritative; a turn never waits on
//! it and never fails because of it.
//!
//! - **Files.** The directory is `0700`; the database and its `-wal` and
//!   `-shm` files are `0600`, checked after open and re-tightened. No path
//!   step is ever a symlink: the directories pass the key module's checks,
//!   the files are opened with `SQLITE_OPEN_NOFOLLOW`, and a symlinked
//!   database or side file is refused (D-091 F-1).
//! - **Opening.** SQLite WAL (set before the schema is migrated), foreign
//!   keys on, the bounded busy handler, and forward-only schema versioning
//!   in an IMMEDIATE transaction ([`super::store_schema`]). A database newer
//!   than this build is recognised through a read-only connection and never
//!   opened for writing, so a downgrade never loses data. A corrupt or
//!   unreadable database is moved aside under a fresh, never-overwritten
//!   name `decisions.db.corrupt-<ts>-<id>` (`0600`), a fresh store is
//!   created and `causal.outcome.incomplete{store_recovered}` is emitted
//!   (V6.10).
//! - **The disabled plane** reads an existing database immutably (no lock,
//!   no `-wal` or `-shm` file) and creates nothing (D-040, D-091 F-5).
//! - **Workspace id.** A store has one stable id, created with its schema
//!   and kept in `store_meta`; every record is stamped with it and every
//!   scope reads it, so moving the workspace changes nothing (D-091 F-13).
//! - **Writes** go to one writer thread per database per process, through a
//!   bounded queue (capacity 1024). Enqueueing never blocks: a full queue
//!   drops the record, which is counted and emitted as
//!   `causal.outcome.incomplete{store_dropped}` (D-040 OQ-A8). Shutdown
//!   drains the queue. A record is idempotent by its primary key: the same
//!   record again is a no-op, and a different record under the same id is a
//!   conflict that is refused and reported, never overwritten.
//! - **Reads** take a [`DecisionScope`]; there is no unscoped read. A
//!   decision outside the scope reads exactly like an unknown one: the same
//!   value on the same path, so there is no existence oracle (V6.04). Rows
//!   without an owner (imported v1 decisions) are visible to the local
//!   operator only. Reads use read-only connections.
//! - **Retention.** Decisions older than the horizon are deleted in batches
//!   of 1000 per transaction, at open and every 10,000 writes; their
//!   outcomes cascade. A horizon of 0 days keeps everything (D-091 F-7).
//!
//! AG-10: the database file is named and opened only here.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Instant;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::ids::DecisionId;
use super::store_schema::{self, SchemaState};
use crate::decision::telemetry::{
    CausalEvent, CausalEventFields, CausalEventKind, CausalEventSink, CausalLineage, IncompleteReason,
};

/// The database file, under `<workspace>/self/causal_tree/`.
const DB_FILE: &str = "decisions.db";

/// Limit the owner-scoped history before the outcome join. The query-plan
/// regression below keeps this read on the existing scope/time index.
const CAPABILITY_LATENCY_SQL: &str = "WITH recent AS (
    SELECT decision_id, catalog_generation, created_at_ms FROM decisions
    WHERE workspace_id = ?1 AND owner_id = ?2
    ORDER BY created_at_ms DESC, decision_id DESC LIMIT 1024
  )
  SELECT o.capability_ids_json, o.tool_calls_json, o.executed_target, o.latency_ms
  FROM recent d JOIN outcomes o ON o.decision_id = d.decision_id
  WHERE d.catalog_generation = ?3 AND o.completeness = 'complete'
    AND o.classification IN ('success', 'fallback_success')
    AND o.executed_action = 'use_tool' AND o.latency_ms IS NOT NULL
  ORDER BY d.created_at_ms DESC, d.decision_id DESC, o.settlement_id DESC LIMIT 1024";

/// The writer queue's capacity (SP-CDP-A 5.10).
const QUEUE_CAPACITY: usize = 1024;

/// Rows deleted per retention transaction.
const PRUNE_BATCH: i64 = 1000;

/// Writes between two retention passes.
const PRUNE_EVERY: u32 = 10_000;

/// Milliseconds in a day.
const DAY_MS: i64 = 86_400_000;

/// Whether the store may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreMode {
    /// Write decisions (the enabled plane).
    ReadWrite,
    /// Read only: the disabled plane keeps its database untouched (D-040).
    ReadOnly,
}

/// Why the store could not be opened or read. The messages carry no row
/// data.
#[derive(Debug, thiserror::Error)]
pub(crate) enum StoreError {
    /// A file operation failed.
    #[error("decision store file operation failed: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite failed.
    #[error("decision store database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A stored JSON column does not decode.
    #[error("decision store row does not decode: {0}")]
    Decode(#[from] serde_json::Error),
    /// A path of the store is a symlink, not a plain file or directory, or
    /// not this user's.
    #[error("decision store path refused: {0}")]
    Unsafe(String),
    #[error("decision recovery read deadline elapsed")]
    RecoveryDeadline,
    #[error("decision recovery record exceeds its byte or field bound")]
    RecoveryBounds,
    #[error("decision recovery schema is unsupported")]
    RecoverySchema,
    #[error("decision recovery store is unavailable")]
    RecoveryUnavailable,
}

/// How long decisions are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Retention {
    days: u32,
}

impl Retention {
    /// `store_days`, or `fitness_days` when that is 0 (CDP 14: the default
    /// horizon is the fitness data's).
    pub(crate) const fn days(store_days: u32, fitness_days: u32) -> Self {
        Self {
            days: if store_days == 0 { fitness_days } else { store_days },
        }
    }

    /// The oldest creation time kept at `now_ms`; `None` keeps everything
    /// (a horizon of 0 days).
    fn cutoff(self, now_ms: i64) -> Option<i64> {
        (self.days > 0).then(|| now_ms.saturating_sub(i64::from(self.days).saturating_mul(DAY_MS)))
    }
}

/// Where a decision belongs.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct RecordLineage {
    pub(crate) workspace_id: Arc<str>,
    /// The preceding decision in this turn's bounded hop chain, if any.
    #[serde(serialize_with = "serialize_prior_decision_id")]
    pub(crate) prior_decision_id: Option<DecisionId>,
    /// `None` only for an imported v1 decision.
    pub(crate) owner_id: Option<Arc<str>>,
    pub(crate) session_key: Option<Arc<str>>,
    pub(crate) run_id: Option<Arc<str>>,
    pub(crate) task_id: Option<Arc<str>>,
    pub(crate) source_event_id: Option<Arc<str>>,
    pub(crate) causation_id: Option<Arc<str>>,
    pub(crate) channel: Option<Arc<str>>,
}

impl std::fmt::Debug for RecordLineage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordLineage")
            .field("has_prior_decision", &self.prior_decision_id.is_some())
            .field("has_owner", &self.owner_id.is_some())
            .field("has_session", &self.session_key.is_some())
            .field("has_run", &self.run_id.is_some())
            .finish_non_exhaustive()
    }
}

fn serialize_prior_decision_id<S: serde::Serializer>(
    prior: &Option<DecisionId>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match prior {
        Some(id) => serializer.serialize_some(&id.to_string()),
        None => serializer.serialize_none(),
    }
}

/// One candidate of a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RecordedCandidate {
    pub(crate) id: String,
    /// The `CausalActionKind` code.
    pub(crate) action: String,
    /// The `CapabilityTarget` code.
    pub(crate) target: String,
    /// The probability the selector gave this candidate, when exploration
    /// was on: the propensity of every eligible candidate (CDP-22 records
    /// it; D-091 F-3). Evaluation's overlap check reads these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) propensity: Option<f64>,
}

/// The validated distributions of a decision. An imported v1 decision has
/// none: it is marked `legacy_v1` and no probability is made up.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct RecordedDistributions {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) legacy_v1: bool,
    /// Choice probabilities of the next action, by action code.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) next_action: BTreeMap<String, f64>,
    /// Choice probabilities of the capability target, by target code.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) capability_target: BTreeMap<String, f64>,
    /// Noul features: the probability of "yes", by feature name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) features: BTreeMap<String, f64>,
}

/// What the builtin shadow chose for the same decision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecordedShadow {
    #[serde(default)]
    pub(crate) next_action: Option<String>,
    #[serde(default)]
    pub(crate) capability_target: Option<String>,
}

/// One backend attempt of a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecordedAttempt {
    /// The backend kind code.
    pub(crate) backend: String,
    /// `served`, `cached`, `failed` or `skipped`.
    pub(crate) outcome: String,
    /// The error class, for a failed or skipped attempt.
    #[serde(default)]
    pub(crate) error_class: Option<String>,
    pub(crate) latency_ms: u64,
}

/// A decision record (CDP 14): every decision field of SP-CDP-A 5.10 except
/// the removed `mode`, plus the `enabled` snapshot. It holds identifiers,
/// codes, a keyed state fingerprint and numbers; never raw state, question
/// text or a goal.
#[derive(Clone, PartialEq)]
pub(crate) struct DecisionRecord {
    pub(crate) decision_id: DecisionId,
    pub(crate) created_at_ms: i64,
    pub(crate) lineage: RecordLineage,
    pub(crate) enabled_snapshot: bool,
    /// A keyed HMAC of the canonical state, never a plain hash of text.
    pub(crate) state_fingerprint: String,
    /// Only when `[causal_tree.store] record_redacted_projection` is on.
    pub(crate) redacted_projection: Option<String>,
    pub(crate) protocol_version: u32,
    pub(crate) policy_version: String,
    pub(crate) catalog_generation: String,
    pub(crate) scorer_version: String,
    pub(crate) backend_kind: String,
    pub(crate) backend_locality: String,
    pub(crate) configured_model: String,
    /// The configured endpoint of the backend, trimmed; empty for a hosted
    /// default, `None` for the builtin and imported rows (D-102 H-2).
    pub(crate) backend_endpoint: Option<String>,
    pub(crate) claimed_model: Option<String>,
    pub(crate) provider_request_id: Option<String>,
    pub(crate) candidates: Vec<RecordedCandidate>,
    /// Both gate passes.
    pub(crate) gates: serde_json::Value,
    pub(crate) distributions: RecordedDistributions,
    pub(crate) builtin_shadow: RecordedShadow,
    pub(crate) features: serde_json::Value,
    pub(crate) selected_candidate_id: Option<String>,
    pub(crate) directive_kind: String,
    pub(crate) fallback_order: Vec<String>,
    pub(crate) abstention_reason: Option<String>,
    pub(crate) propensity: f64,
    pub(crate) explored: bool,
    pub(crate) latency_ms: u64,
    pub(crate) usage: Option<serde_json::Value>,
    pub(crate) attempts: Vec<RecordedAttempt>,
    pub(crate) error_class: Option<String>,
    pub(crate) cache_hit: bool,
    pub(crate) response_sha256: Option<String>,
    pub(crate) legacy_v1: bool,
}

impl std::fmt::Debug for DecisionRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionRecord")
            .field("decision_id", &self.decision_id)
            .field("lineage", &self.lineage)
            .field("candidate_count", &self.candidates.len())
            .field("attempt_count", &self.attempts.len())
            .field("has_backend_endpoint", &self.backend_endpoint.is_some())
            .field("has_redacted_projection", &self.redacted_projection.is_some())
            .finish_non_exhaustive()
    }
}

/// The observed terminal classification, never inferred from the predicted action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutcomeClassification {
    /// Dispatch completed; child completion is represented by separate links.
    Delegated,
    AwaitingApproval,
    Silent,
    Success,
    FallbackSuccess,
    Refusal,
    Clarification,
    Cancelled,
    Failed,
}

impl OutcomeClassification {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Delegated => "delegated",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Silent => "silent",
            Self::Success => "success",
            Self::FallbackSuccess => "fallback_success",
            Self::Refusal => "refusal",
            Self::Clarification => "clarification",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutcomeCompleteness {
    Complete,
    Partial,
}

impl OutcomeCompleteness {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
        }
    }
}

/// Metadata from an actual provider attempt; error messages and response text are excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecordedOutcomeAttempt {
    pub(crate) seq: u32,
    pub(crate) status: String,
    pub(crate) error_class: Option<String>,
}

/// Canonical capability and outcome code only; never tool arguments or results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecordedToolCall {
    pub(crate) capability_id: String,
    pub(crate) status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RecordedApproval {
    pub(crate) op_digest: String,
    pub(crate) status: String,
}

/// One terminal settlement. `None` means unavailable observation; an empty
/// vector means the source observed zero items. Raw turn text is not retained.
#[derive(Clone, PartialEq)]
pub(crate) struct OutcomeRecord {
    pub(super) decision_id: DecisionId,
    pub(super) settlement_id: String,
    pub(super) recorded_at_ms: i64,
    pub(super) classification: OutcomeClassification,
    pub(super) executed_action: Option<String>,
    pub(super) executed_target: Option<String>,
    pub(super) provider_attempts: Option<Vec<RecordedOutcomeAttempt>>,
    pub(super) tool_calls: Option<Vec<RecordedToolCall>>,
    pub(super) capability_ids: Option<Vec<String>>,
    pub(super) approvals: Option<Vec<RecordedApproval>>,
    pub(super) tokens_in: Option<u64>,
    pub(super) tokens_out: Option<u64>,
    pub(super) latency_ms: Option<u64>,
    pub(super) cost_micro_usd: Option<i64>,
    pub(super) idempotency_status: Option<String>,
    pub(super) reward: Option<BTreeMap<String, f64>>,
    pub(super) prompt_bytes_before: Option<u64>,
    pub(super) prompt_bytes_after: Option<u64>,
    pub(super) tool_bytes_before: Option<u64>,
    pub(super) tool_bytes_after: Option<u64>,
    pub(super) denied_tool_attempts: Option<u64>,
    pub(super) completeness: OutcomeCompleteness,
    pub(super) coverage: Option<f64>,
}

impl std::fmt::Debug for OutcomeRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutcomeRecord")
            .field("decision_id", &self.decision_id)
            .field("classification", &self.classification)
            .field("completeness", &self.completeness)
            .finish_non_exhaustive()
    }
}

pub(crate) const MAX_RECORDED_PROVIDER_ATTEMPTS: usize = 128;
const MAX_OUTCOME_STORED_BYTES: usize = 64 * 1024;

fn bounded_code(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

fn bounded_capability_id(value: &str) -> bool {
    if value.is_empty() || value.len() > 192 {
        return false;
    }
    let mut parts = value.split(':');
    let (Some(family), Some(namespace), Some(public_name), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    super::branch::CapabilityTarget::ALL
        .iter()
        .find(|target| target.as_str() == family)
        .and_then(|target| super::branch::CapabilityId::new(*target, namespace, public_name).ok())
        .is_some_and(|canonical| canonical.as_str() == value)
}

impl OutcomeRecord {
    /// Reject unbounded or user-shaped metadata before the bounded queue.
    /// The causal correlator still owns source verification and canonical IDs.
    fn valid_for_queue(&self) -> bool {
        if !bounded_code(&self.settlement_id, 128)
            || self.recorded_at_ms <= 0
            || self.cost_micro_usd.is_some_and(|cost| cost < 0)
            || self
                .coverage
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            || self.reward.as_ref().is_some_and(|reward| {
                reward.len() > 16
                    || reward
                        .iter()
                        .any(|(key, value)| !bounded_code(key, 64) || !value.is_finite())
            })
            || [
                self.tokens_in,
                self.tokens_out,
                self.latency_ms,
                self.prompt_bytes_before,
                self.prompt_bytes_after,
                self.tool_bytes_before,
                self.tool_bytes_after,
                self.denied_tool_attempts,
            ]
            .into_iter()
            .flatten()
            .any(|value| value > i64::MAX as u64)
        {
            return false;
        }
        let valid_action = self.executed_action.as_ref().is_none_or(|action| {
            super::branch::CausalActionKind::ALL
                .iter()
                .any(|candidate| candidate.as_str() == action)
        });
        let valid_target = self.executed_target.as_ref().is_none_or(|target| {
            super::branch::CapabilityTarget::ALL
                .iter()
                .any(|candidate| candidate.as_str() == target)
        });
        if !valid_action || !valid_target || self.executed_action.is_some() != self.executed_target.is_some() {
            return false;
        }
        match self.classification {
            OutcomeClassification::Silent
                if self.executed_action.is_some()
                    || self.executed_target.is_some()
                    || self.completeness != OutcomeCompleteness::Partial =>
            {
                return false;
            }
            OutcomeClassification::Delegated
                if self.executed_action.as_deref() != Some("delegate")
                    || self.executed_target.as_deref() != Some("session")
                    || self.completeness != OutcomeCompleteness::Partial =>
            {
                return false;
            }
            OutcomeClassification::Refusal
                if self.executed_action.as_deref() != Some("refuse")
                    || self.executed_target.as_deref() != Some("none") =>
            {
                return false;
            }
            OutcomeClassification::Clarification
                if self.executed_action.as_deref() != Some("clarify")
                    || self.executed_target.as_deref() != Some("none") =>
            {
                return false;
            }
            OutcomeClassification::Success | OutcomeClassification::FallbackSuccess
                if self.executed_action.is_none()
                    || matches!(self.executed_action.as_deref(), Some("refuse" | "clarify")) =>
            {
                return false;
            }
            _ => {}
        }
        if self.completeness == OutcomeCompleteness::Complete
            && (self.provider_attempts.is_none()
                || self.tool_calls.is_none()
                || self.capability_ids.is_none()
                || self.approvals.is_none()
                || self.coverage.is_none())
        {
            return false;
        }
        if self.idempotency_status.as_deref().is_some_and(|status| {
            !matches!(
                status,
                "recorded" | "replayed" | "disabled" | "unknown_pricing" | "failed"
            )
        }) {
            return false;
        }
        if self.provider_attempts.as_ref().is_some_and(|attempts| {
            attempts.len() > MAX_RECORDED_PROVIDER_ATTEMPTS
                || attempts.iter().enumerate().any(|(index, attempt)| {
                    attempt.seq != u32::try_from(index + 1).unwrap_or(u32::MAX)
                        || !matches!(attempt.status.as_str(), "success" | "failed" | "skipped")
                        || attempt
                            .error_class
                            .as_deref()
                            .is_some_and(|class| !bounded_code(class, 64))
                        || (attempt.status == "success" && attempt.error_class.is_some())
                })
        }) {
            return false;
        }
        if self.tool_calls.as_ref().is_some_and(|calls| {
            calls.len() > 64
                || calls.iter().any(|call| {
                    !bounded_capability_id(&call.capability_id)
                        || !matches!(
                            call.status.as_str(),
                            "success" | "failed" | "denied" | "cancelled" | "awaiting_approval"
                        )
                })
        }) {
            return false;
        }
        if self
            .capability_ids
            .as_ref()
            .is_some_and(|ids| ids.len() > 64 || ids.iter().any(|id| !bounded_capability_id(id)))
        {
            return false;
        }
        if self.approvals.as_ref().is_some_and(|approvals| {
            approvals.len() > 64
                || approvals.iter().any(|approval| {
                    approval.op_digest.len() != 64
                        || !approval.op_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                        || !matches!(
                            approval.status.as_str(),
                            "pending" | "approved" | "denied" | "expired" | "consumed"
                        )
                })
        }) {
            return false;
        }
        EncodedOutcome::of(self).is_ok_and(|encoded| {
            encoded
                .columns
                .iter()
                .map(|(_, value)| match value {
                    rusqlite::types::Value::Text(text) => text.len(),
                    rusqlite::types::Value::Blob(bytes) => bytes.len(),
                    _ => 8,
                })
                .sum::<usize>()
                <= MAX_OUTCOME_STORED_BYTES
        })
    }
}

/// The version of the record hash: the columns it covers are
/// [`RECORD_HASH_COLUMNS`], fixed for this version (D-105 K-2).
const RECORD_HASH_DOMAIN: &[u8] = b"prx.decision.record_hash.v1\0";

/// The columns the record hash covers, in order: the schema-1 decision
/// columns. A column a later schema adds is stored but not hashed, so a
/// record written before it and sent again still compares equal; a new
/// hash version is a deliberate change of this list and of
/// [`RECORD_HASH_DOMAIN`].
const RECORD_HASH_COLUMNS: [&str; 40] = [
    "decision_id",
    "created_at_ms",
    "workspace_id",
    "owner_id",
    "session_key",
    "run_id",
    "task_id",
    "source_event_id",
    "causation_id",
    "channel",
    "enabled_snapshot",
    "state_fingerprint",
    "redacted_projection",
    "protocol_version",
    "policy_version",
    "catalog_generation",
    "scorer_version",
    "backend_kind",
    "backend_locality",
    "configured_model",
    "claimed_model",
    "provider_request_id",
    "candidates_json",
    "gates_json",
    "distributions_json",
    "builtin_shadow_json",
    "features_json",
    "selected_candidate_id",
    "directive_kind",
    "fallback_order_json",
    "abstention_reason",
    "propensity",
    "explored",
    "latency_ms",
    "usage_json",
    "attempts_json",
    "error_class",
    "cache_hit",
    "response_sha256",
    "legacy_v1",
];

/// Canonical typed-column encoding shared by the issued record and the
/// persisted row. The durable `record_hash` is deliberately not trusted.
fn replay_column_hash(digest: &mut Sha256, name: &str, value: rusqlite::types::ValueRef<'_>) {
    use rusqlite::types::ValueRef;
    digest.update((name.len() as u64).to_be_bytes());
    digest.update(name.as_bytes());
    match value {
        ValueRef::Null => digest.update(b"n"),
        ValueRef::Integer(value) => {
            digest.update(b"i");
            digest.update(value.to_be_bytes());
        }
        ValueRef::Real(value) => {
            digest.update(b"r");
            digest.update(value.to_bits().to_be_bytes());
        }
        ValueRef::Text(value) => {
            digest.update(b"t");
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value);
        }
        ValueRef::Blob(value) => {
            digest.update(b"b");
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value);
        }
    }
}

impl DecisionRecord {
    pub(crate) fn replay_digest(&self, store_workspace_id: &str) -> Result<String, StoreError> {
        let encoded = EncodedDecision::of(self)?;
        let mut digest = Sha256::new();
        digest.update(b"prx.decision.terminal-replay-row.v1\0");
        for name in RECORD_HASH_COLUMNS
            .iter()
            .copied()
            .chain(["prior_decision_id", "backend_endpoint"])
        {
            let (_, value) = encoded
                .columns
                .iter()
                .find(|(column, _)| *column == name)
                .ok_or_else(|| StoreError::Unsafe("incomplete decision replay encoding".to_owned()))?;
            // The writer replaces the runtime path with its durable store ID.
            if name == "workspace_id" {
                replay_column_hash(
                    &mut digest,
                    name,
                    rusqlite::types::ValueRef::Text(store_workspace_id.as_bytes()),
                );
            } else {
                replay_column_hash(&mut digest, name, value.as_value_ref());
            }
        }
        Ok(hex::encode(digest.finalize()))
    }
}

/// Decision fields borrow their record; only generated UUID/JSON text is owned.
/// This encoding never escapes the synchronous hash/SQLite call.
enum DecisionValue<'a> {
    Null,
    Integer(i64),
    Real(f64),
    Text(std::borrow::Cow<'a, str>),
}

impl DecisionValue<'_> {
    fn as_value_ref(&self) -> rusqlite::types::ValueRef<'_> {
        use rusqlite::types::ValueRef;
        match self {
            Self::Null => ValueRef::Null,
            Self::Integer(value) => ValueRef::Integer(*value),
            Self::Real(value) => ValueRef::Real(*value),
            Self::Text(value) => ValueRef::Text(value.as_bytes()),
        }
    }
}

fn decision_text(value: Option<&str>) -> DecisionValue<'_> {
    value.map_or(DecisionValue::Null, |value| {
        DecisionValue::Text(std::borrow::Cow::Borrowed(value))
    })
}

/// A record in its fixed column form, with its original versioned hash.
struct EncodedDecision<'a> {
    columns: [(&'static str, DecisionValue<'a>); 42],
    record_hash: String,
}

fn text(value: Option<&str>) -> rusqlite::types::Value {
    value.map_or(rusqlite::types::Value::Null, |value| {
        rusqlite::types::Value::Text(value.to_string())
    })
}

fn integer(value: impl TryInto<i64>) -> rusqlite::types::Value {
    rusqlite::types::Value::Integer(value.try_into().unwrap_or(i64::MAX))
}

impl<'a> EncodedDecision<'a> {
    fn of(record: &'a DecisionRecord) -> Result<Self, serde_json::Error> {
        use DecisionValue as V;
        let lineage = &record.lineage;
        let columns = [
            ("decision_id", V::Text(record.decision_id.to_string().into())),
            ("created_at_ms", V::Integer(record.created_at_ms)),
            (
                "workspace_id",
                V::Text(std::borrow::Cow::Borrowed(&lineage.workspace_id)),
            ),
            (
                "prior_decision_id",
                lineage
                    .prior_decision_id
                    .map_or(V::Null, |id| V::Text(id.to_string().into())),
            ),
            ("owner_id", decision_text(lineage.owner_id.as_deref())),
            ("session_key", decision_text(lineage.session_key.as_deref())),
            ("run_id", decision_text(lineage.run_id.as_deref())),
            ("task_id", decision_text(lineage.task_id.as_deref())),
            ("source_event_id", decision_text(lineage.source_event_id.as_deref())),
            ("causation_id", decision_text(lineage.causation_id.as_deref())),
            ("channel", decision_text(lineage.channel.as_deref())),
            ("enabled_snapshot", V::Integer(i64::from(record.enabled_snapshot))),
            (
                "state_fingerprint",
                V::Text(std::borrow::Cow::Borrowed(&record.state_fingerprint)),
            ),
            (
                "redacted_projection",
                decision_text(record.redacted_projection.as_deref()),
            ),
            ("protocol_version", V::Integer(i64::from(record.protocol_version))),
            (
                "policy_version",
                V::Text(std::borrow::Cow::Borrowed(&record.policy_version)),
            ),
            (
                "catalog_generation",
                V::Text(std::borrow::Cow::Borrowed(&record.catalog_generation)),
            ),
            (
                "scorer_version",
                V::Text(std::borrow::Cow::Borrowed(&record.scorer_version)),
            ),
            (
                "backend_kind",
                V::Text(std::borrow::Cow::Borrowed(&record.backend_kind)),
            ),
            (
                "backend_locality",
                V::Text(std::borrow::Cow::Borrowed(&record.backend_locality)),
            ),
            (
                "configured_model",
                V::Text(std::borrow::Cow::Borrowed(&record.configured_model)),
            ),
            ("backend_endpoint", decision_text(record.backend_endpoint.as_deref())),
            ("claimed_model", decision_text(record.claimed_model.as_deref())),
            (
                "provider_request_id",
                decision_text(record.provider_request_id.as_deref()),
            ),
            (
                "candidates_json",
                V::Text(serde_json::to_string(&record.candidates)?.into()),
            ),
            ("gates_json", V::Text(serde_json::to_string(&record.gates)?.into())),
            (
                "distributions_json",
                V::Text(serde_json::to_string(&record.distributions)?.into()),
            ),
            (
                "builtin_shadow_json",
                V::Text(serde_json::to_string(&record.builtin_shadow)?.into()),
            ),
            (
                "features_json",
                V::Text(serde_json::to_string(&record.features)?.into()),
            ),
            (
                "selected_candidate_id",
                decision_text(record.selected_candidate_id.as_deref()),
            ),
            (
                "directive_kind",
                V::Text(std::borrow::Cow::Borrowed(&record.directive_kind)),
            ),
            (
                "fallback_order_json",
                V::Text(serde_json::to_string(&record.fallback_order)?.into()),
            ),
            ("abstention_reason", decision_text(record.abstention_reason.as_deref())),
            ("propensity", V::Real(record.propensity)),
            ("explored", V::Integer(i64::from(record.explored))),
            (
                "latency_ms",
                V::Integer(i64::try_from(record.latency_ms).unwrap_or(i64::MAX)),
            ),
            (
                "usage_json",
                match &record.usage {
                    Some(usage) => V::Text(serde_json::to_string(usage)?.into()),
                    None => V::Null,
                },
            ),
            (
                "attempts_json",
                V::Text(serde_json::to_string(&record.attempts)?.into()),
            ),
            ("error_class", decision_text(record.error_class.as_deref())),
            ("cache_hit", V::Integer(i64::from(record.cache_hit))),
            ("response_sha256", decision_text(record.response_sha256.as_deref())),
            ("legacy_v1", V::Integer(i64::from(record.legacy_v1))),
        ];
        // The hash covers a fixed, versioned list of columns in a fixed
        // order, so the same record always hashes the same (the idempotency
        // comparison), and a column a later schema adds never turns a resent
        // record into a false conflict (D-105 K-2).
        let mut hasher = Sha256::new();
        hasher.update(RECORD_HASH_DOMAIN);
        for (name, value) in columns.iter().filter(|(name, _)| RECORD_HASH_COLUMNS.contains(name)) {
            hasher.update(name.as_bytes());
            hasher.update([0]);
            match value.as_value_ref() {
                rusqlite::types::ValueRef::Null => hasher.update(b"n"),
                rusqlite::types::ValueRef::Integer(number) => {
                    hasher.update(b"i");
                    hasher.update(number.to_be_bytes());
                }
                rusqlite::types::ValueRef::Real(number) => {
                    hasher.update(b"r");
                    hasher.update(number.to_bits().to_be_bytes());
                }
                rusqlite::types::ValueRef::Text(text) => {
                    hasher.update(b"t");
                    hasher.update((text.len() as u64).to_be_bytes());
                    hasher.update(text);
                }
                rusqlite::types::ValueRef::Blob(bytes) => {
                    hasher.update(b"b");
                    hasher.update((bytes.len() as u64).to_be_bytes());
                    hasher.update(bytes);
                }
            }
        }
        Ok(Self {
            columns,
            record_hash: hex::encode(hasher.finalize()),
        })
    }
}

const OUTCOME_HASH_DOMAIN: &[u8] = b"prx.outcome.record_hash.v1\0";

struct EncodedOutcome {
    columns: Vec<(&'static str, rusqlite::types::Value)>,
    record_hash: String,
}

fn optional_json<T: Serialize>(value: Option<&T>) -> Result<rusqlite::types::Value, serde_json::Error> {
    value.map_or_else(
        || Ok(rusqlite::types::Value::Null),
        |value| serde_json::to_string(value).map(rusqlite::types::Value::Text),
    )
}

impl EncodedOutcome {
    fn of(record: &OutcomeRecord) -> Result<Self, serde_json::Error> {
        use rusqlite::types::Value as V;
        let columns = vec![
            ("decision_id", V::Text(record.decision_id.to_string())),
            ("settlement_id", V::Text(record.settlement_id.clone())),
            ("recorded_at_ms", V::Integer(record.recorded_at_ms)),
            ("classification", V::Text(record.classification.as_str().to_owned())),
            ("executed_action", text(record.executed_action.as_deref())),
            ("executed_target", text(record.executed_target.as_deref())),
            (
                "provider_attempts_json",
                optional_json(record.provider_attempts.as_ref())?,
            ),
            ("tool_calls_json", optional_json(record.tool_calls.as_ref())?),
            ("capability_ids_json", optional_json(record.capability_ids.as_ref())?),
            ("approvals_json", optional_json(record.approvals.as_ref())?),
            ("tokens_in", record.tokens_in.map_or(V::Null, integer)),
            ("tokens_out", record.tokens_out.map_or(V::Null, integer)),
            ("latency_ms", record.latency_ms.map_or(V::Null, integer)),
            ("cost_micro_usd", record.cost_micro_usd.map_or(V::Null, V::Integer)),
            ("idempotency_status", text(record.idempotency_status.as_deref())),
            ("reward_json", optional_json(record.reward.as_ref())?),
            (
                "prompt_bytes_before",
                record.prompt_bytes_before.map_or(V::Null, integer),
            ),
            ("prompt_bytes_after", record.prompt_bytes_after.map_or(V::Null, integer)),
            ("tool_bytes_before", record.tool_bytes_before.map_or(V::Null, integer)),
            ("tool_bytes_after", record.tool_bytes_after.map_or(V::Null, integer)),
            (
                "denied_tool_attempts",
                record.denied_tool_attempts.map_or(V::Null, integer),
            ),
            ("completeness", V::Text(record.completeness.as_str().to_owned())),
            ("coverage", record.coverage.map_or(V::Null, V::Real)),
        ];
        let mut hasher = Sha256::new();
        hasher.update(OUTCOME_HASH_DOMAIN);
        // Arrival time is observational metadata. The same terminal receipt
        // may be rejoined after restart at a later time; its facts must still
        // compare equal while the first stored timestamp remains untouched.
        for (name, value) in columns.iter().filter(|(name, _)| *name != "recorded_at_ms") {
            hasher.update(name.as_bytes());
            hasher.update([0]);
            match value {
                V::Null => hasher.update(b"n"),
                V::Integer(number) => {
                    hasher.update(b"i");
                    hasher.update(number.to_be_bytes());
                }
                V::Real(number) => {
                    hasher.update(b"r");
                    hasher.update(number.to_bits().to_be_bytes());
                }
                V::Text(text) => {
                    hasher.update(b"t");
                    hasher.update((text.len() as u64).to_be_bytes());
                    hasher.update(text.as_bytes());
                }
                V::Blob(bytes) => {
                    hasher.update(b"b");
                    hasher.update((bytes.len() as u64).to_be_bytes());
                    hasher.update(bytes);
                }
            }
        }
        Ok(Self {
            columns,
            record_hash: hex::encode(hasher.finalize()),
        })
    }
}

/// Whether a record was queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Enqueue {
    /// The writer will store it.
    Queued,
    /// The queue was full or closed; the record is lost (counted, reported).
    Dropped,
}

/// The import progress of one v1 feedback file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportProgress {
    pub(crate) file: String,
    pub(crate) digest: String,
    pub(crate) lines: u64,
}

impl ImportProgress {
    fn meta_key(file: &str) -> String {
        format!("v1_import:{file}")
    }

    fn meta_value(&self) -> String {
        format!("{} {}", self.digest, self.lines)
    }
}

mod child_recovery;
pub(crate) use child_recovery::{ChildRecoveryCursor, ChildRecoveryLineage};
mod child_outcome;
pub(crate) use child_outcome::{ChildOutcomeLink, ChildOutcomeView};

enum WriterMessage {
    ChildOutcome(Box<ChildOutcomeLink>),
    DecisionBatch(Box<[DecisionRecord]>),
    Outcome(Box<OutcomeRecord>),
    /// A v1 file's progress, saved only when every listed decision is
    /// stored (a record the writer failed to store is never marked done).
    Imported(ImportProgress, Vec<DecisionId>),
    Drain(tokio::sync::oneshot::Sender<()>),
    /// Checkpoint the WAL, close the connection and stop the writer; the
    /// sender is answered once the connection is closed.
    Close(tokio::sync::oneshot::Sender<()>),
}

/// The writer's queue. Cloning shares the one writer thread.
#[derive(Clone)]
pub(crate) struct DecisionStoreWriter {
    queue: SyncSender<WriterMessage>,
    events: Arc<dyn CausalEventSink>,
    workspace_id: Arc<str>,
}

impl std::fmt::Debug for DecisionStoreWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionStoreWriter").finish_non_exhaustive()
    }
}

impl DecisionStoreWriter {
    /// Queue `record`. Never blocks: when the queue is full the record is
    /// dropped, counted and reported (D-040 OQ-A8).
    pub(crate) fn record_decision(&self, record: DecisionRecord) -> Enqueue {
        self.record_chain(vec![record])
    }

    /// Admit one bounded hop chain as a single queue item. The writer uses
    /// one SQLite transaction; a full queue loses the entire chain.
    pub(crate) fn record_chain(&self, mut records: Vec<DecisionRecord>) -> Enqueue {
        if records.is_empty() || records.len() > 5 || !same_turn_chain(&records) {
            for record in records {
                emit_incomplete(self.events.as_ref(), lineage_of(&record), IncompleteReason::Conflict);
            }
            return Enqueue::Dropped;
        }
        // The store's own id is the one scope key (D-091 F-13).
        for record in &mut records {
            record.lineage.workspace_id = Arc::clone(&self.workspace_id);
        }
        match self
            .queue
            .try_send(WriterMessage::DecisionBatch(records.into_boxed_slice()))
        {
            Ok(()) => Enqueue::Queued,
            Err(refused) => {
                tracing::warn!("decision store queue is full or closed; a decision chain was dropped");
                let (TrySendError::Full(command) | TrySendError::Disconnected(command)) = refused;
                if let WriterMessage::DecisionBatch(records) = command {
                    for record in &records {
                        emit_incomplete(self.events.as_ref(), lineage_of(record), IncompleteReason::StoreDropped);
                    }
                }
                Enqueue::Dropped
            }
        }
    }

    /// Admit one observed terminal settlement without waiting on SQLite.
    /// A late settlement may arrive after a process restart; the writer
    /// verifies its decision exists in this workspace before inserting it.
    pub(crate) fn record_outcome(&self, record: OutcomeRecord) -> Enqueue {
        let lineage = CausalLineage {
            workspace_id: Arc::clone(&self.workspace_id),
            decision_id: Some(Arc::from(record.decision_id.to_string())),
            ..CausalLineage::default()
        };
        if !record.valid_for_queue() {
            emit_incomplete(self.events.as_ref(), lineage, IncompleteReason::Conflict);
            return Enqueue::Dropped;
        }
        let decision_id = record.decision_id;
        match self.queue.try_send(WriterMessage::Outcome(Box::new(record))) {
            Ok(()) => Enqueue::Queued,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                tracing::warn!("decision store queue is full or closed; an outcome was dropped");
                emit_incomplete(
                    self.events.as_ref(),
                    CausalLineage {
                        workspace_id: Arc::clone(&self.workspace_id),
                        decision_id: Some(Arc::from(decision_id.to_string())),
                        ..CausalLineage::default()
                    },
                    IncompleteReason::StoreDropped,
                );
                Enqueue::Dropped
            }
        }
    }

    /// Queue the import progress of one v1 file, after its records; it is
    /// saved only when every decision of `imported` is stored (D-091 F-11).
    pub(crate) fn mark_imported(&self, progress: ImportProgress, imported: Vec<DecisionId>) -> Enqueue {
        match self.queue.try_send(WriterMessage::Imported(progress, imported)) {
            Ok(()) => Enqueue::Queued,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Enqueue::Dropped,
        }
    }

    /// Wait, until `deadline` at most, for everything queued before this
    /// call to be written (shutdown, and the importer's pacing).
    pub(crate) async fn drain(&self, deadline: Instant) -> bool {
        self.send_and_wait(WriterMessage::Drain, deadline).await
    }

    /// Write everything queued, checkpoint the WAL and close the writer's
    /// connection, waiting until `deadline` at most. Later records are
    /// dropped and reported.
    async fn close(&self, deadline: Instant) {
        let _ = self.send_and_wait(WriterMessage::Close, deadline).await;
    }

    async fn send_and_wait(
        &self,
        message: fn(tokio::sync::oneshot::Sender<()>) -> WriterMessage,
        deadline: Instant,
    ) -> bool {
        let (done, finished) = tokio::sync::oneshot::channel();
        let mut command = message(done);
        loop {
            match self.queue.try_send(command) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    command = returned;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        matches!(tokio::time::timeout_at(deadline.into(), finished).await, Ok(Ok(())))
    }
}

/// A chain is ordered, unique and owned by one source turn. The root has no
/// prior hop; child-worker parentage uses a separate lineage field.
pub(crate) fn same_turn_chain(records: &[DecisionRecord]) -> bool {
    let Some(root) = records.first().map(|record| &record.lineage) else {
        return false;
    };
    let mut previous = None;
    let mut ids = std::collections::BTreeSet::new();
    for record in records {
        let lineage = &record.lineage;
        if !ids.insert(record.decision_id)
            || lineage.workspace_id != root.workspace_id
            || lineage.owner_id != root.owner_id
            || lineage.session_key != root.session_key
            || lineage.run_id != root.run_id
            || lineage.task_id != root.task_id
            || lineage.source_event_id != root.source_event_id
            || lineage.causation_id != root.causation_id
            || lineage.channel != root.channel
            || lineage.prior_decision_id != previous
        {
            return false;
        }
        previous = Some(record.decision_id);
    }
    true
}

/// The event lineage of `record`.
fn lineage_of(record: &DecisionRecord) -> CausalLineage {
    let lineage = &record.lineage;
    CausalLineage {
        workspace_id: Arc::clone(&lineage.workspace_id),
        owner_id: lineage.owner_id.clone(),
        session_key: lineage.session_key.clone(),
        run_id: lineage.run_id.clone(),
        task_id: lineage.task_id.clone(),
        source_event_id: lineage.source_event_id.clone(),
        causation_event_id: lineage.causation_id.clone(),
        decision_id: Some(Arc::from(record.decision_id.to_string())),
    }
}

fn emit_incomplete(events: &dyn CausalEventSink, lineage: CausalLineage, reason: IncompleteReason) {
    events.emit(CausalEvent {
        kind: CausalEventKind::OutcomeIncomplete,
        lineage,
        fields: CausalEventFields {
            incomplete: Some(reason),
            ..CausalEventFields::default()
        },
    });
}

/// Who may read which decisions (V6.04).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecisionScope {
    /// One owner's decisions in a workspace.
    Owner { workspace_id: Arc<str>, owner_id: Arc<str> },
    /// Every decision of the workspace, including those without an owner:
    /// the local operator (D-019 Q-5).
    LocalOperator { workspace_id: Arc<str> },
}

impl DecisionScope {
    /// The scope's `WHERE` clause over `decisions` (aliased `d`), with its
    /// two parameters.
    fn clause(&self) -> (&'static str, [&str; 2]) {
        match self {
            Self::Owner { workspace_id, owner_id } => (
                "d.workspace_id = ?2 AND d.owner_id = ?3",
                [workspace_id.as_ref(), owner_id.as_ref()],
            ),
            Self::LocalOperator { workspace_id } => ("d.workspace_id = ?2 AND ?3 = ?3", [workspace_id.as_ref(), ""]),
        }
    }
}

/// A half-open time window `[from_ms, to_ms)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimeWindow {
    pub(crate) from_ms: i64,
    pub(crate) to_ms: i64,
}

/// One outcome of a decision, as read back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OutcomeView {
    pub(crate) settlement_id: String,
    pub(crate) classification: String,
    pub(crate) executed_action: Option<String>,
    pub(crate) executed_target: Option<String>,
    pub(crate) provider_attempts: u64,
    pub(crate) cost_micro_usd: Option<i64>,
    pub(crate) reward: Option<serde_json::Value>,
    pub(crate) prompt_bytes_before: Option<i64>,
    pub(crate) prompt_bytes_after: Option<i64>,
    pub(crate) tool_bytes_before: Option<i64>,
    pub(crate) tool_bytes_after: Option<i64>,
    pub(crate) denied_tool_attempts: Option<i64>,
    pub(crate) completeness: String,
}

/// A decision as read back, with its outcomes.
#[derive(Clone, PartialEq)]
pub(crate) struct DecisionView {
    pub(crate) decision_id: DecisionId,
    pub(crate) prior_decision_id: Option<DecisionId>,
    pub(crate) created_at_ms: i64,
    pub(crate) has_owner: bool,
    pub(crate) session_key: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) task_id: Option<String>,
    pub(crate) channel: Option<String>,
    pub(crate) enabled_snapshot: bool,
    pub(crate) policy_version: String,
    pub(crate) scorer_version: String,
    pub(crate) backend_kind: String,
    pub(crate) backend_locality: String,
    pub(crate) configured_model: String,
    pub(crate) backend_endpoint: Option<String>,
    pub(crate) candidates: Vec<RecordedCandidate>,
    pub(crate) distributions: RecordedDistributions,
    pub(crate) builtin_shadow: RecordedShadow,
    pub(crate) selected_candidate_id: Option<String>,
    pub(crate) directive_kind: String,
    pub(crate) fallback_order: Vec<String>,
    pub(crate) abstention_reason: Option<String>,
    pub(crate) propensity: f64,
    pub(crate) explored: bool,
    pub(crate) latency_ms: i64,
    pub(crate) attempts: Vec<RecordedAttempt>,
    pub(crate) error_class: Option<String>,
    pub(crate) cache_hit: bool,
    pub(crate) legacy_v1: bool,
    pub(crate) outcomes: Vec<OutcomeView>,
}

impl std::fmt::Debug for DecisionView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionView")
            .field("decision_id", &self.decision_id)
            .field("has_prior_decision", &self.prior_decision_id.is_some())
            .field("has_owner", &self.has_owner)
            .field("candidate_count", &self.candidates.len())
            .field("outcome_count", &self.outcomes.len())
            .field("has_backend_endpoint", &self.backend_endpoint.is_some())
            .finish_non_exhaustive()
    }
}

impl DecisionView {
    /// The candidate the decision selected.
    pub(crate) fn selected(&self) -> Option<&RecordedCandidate> {
        let selected = self.selected_candidate_id.as_deref()?;
        self.candidates.iter().find(|candidate| candidate.id == selected)
    }

    /// Whether an already-verified child grant may name this decision as its
    /// parent. Direct Answer turns can publish a bounded delegator and issue
    /// the same sealed child grant as a selected Delegate turn; callers must
    /// still verify that grant, its digest and all source lineage separately.
    pub(crate) fn admits_issued_child(&self) -> bool {
        matches!(self.directive_kind.as_str(), "delegate" | "answer")
    }
}

/// The rows evaluation reads (CDP-10): the non-legacy decisions of a window
/// with their outcomes, and how many legacy rows were left out.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EvaluationRows {
    pub(crate) decisions: Vec<DecisionView>,
    pub(crate) legacy_excluded: u64,
}

const DECISION_COLUMNS_BASE: &str = "d.decision_id, d.created_at_ms, d.owner_id IS NOT NULL, d.run_id, d.task_id, \
     d.channel, d.enabled_snapshot, d.policy_version, d.scorer_version, d.backend_kind, d.backend_locality, \
     d.configured_model, d.candidates_json, d.distributions_json, d.builtin_shadow_json, d.selected_candidate_id, \
     d.directive_kind, d.fallback_order_json, d.abstention_reason, d.propensity, d.explored, d.latency_ms, \
     d.attempts_json, d.error_class, d.cache_hit, d.legacy_v1";

/// Disabled/read-only stores stay unmodified, including older schemas. Read
/// their absent trailing columns as NULL while preserving every column index.
fn decision_columns(conn: &Connection) -> Result<String, StoreError> {
    let version = store_schema::version(conn)?;
    let endpoint = if version >= 2 {
        "d.backend_endpoint"
    } else {
        "NULL AS backend_endpoint"
    };
    let prior = if version >= 3 {
        "d.prior_decision_id"
    } else {
        "NULL AS prior_decision_id"
    };
    Ok(format!("{DECISION_COLUMNS_BASE}, {endpoint}, {prior}, d.session_key"))
}

/// A decision row before its JSON columns are decoded.
struct RawDecision {
    id: String,
    prior_decision_id: Option<String>,
    created_at_ms: i64,
    has_owner: bool,
    session_key: Option<String>,
    run_id: Option<String>,
    task_id: Option<String>,
    channel: Option<String>,
    enabled_snapshot: bool,
    policy_version: String,
    scorer_version: String,
    backend_kind: String,
    backend_locality: String,
    configured_model: String,
    candidates: String,
    distributions: String,
    shadow: String,
    selected_candidate_id: Option<String>,
    directive_kind: String,
    fallback_order: String,
    abstention_reason: Option<String>,
    propensity: f64,
    explored: bool,
    latency_ms: i64,
    attempts: String,
    error_class: Option<String>,
    cache_hit: bool,
    legacy_v1: bool,
    backend_endpoint: Option<String>,
}

impl RawDecision {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            created_at_ms: row.get(1)?,
            has_owner: row.get(2)?,
            run_id: row.get(3)?,
            task_id: row.get(4)?,
            channel: row.get(5)?,
            enabled_snapshot: row.get(6)?,
            policy_version: row.get(7)?,
            scorer_version: row.get(8)?,
            backend_kind: row.get(9)?,
            backend_locality: row.get(10)?,
            configured_model: row.get(11)?,
            candidates: row.get(12)?,
            distributions: row.get(13)?,
            shadow: row.get(14)?,
            selected_candidate_id: row.get(15)?,
            directive_kind: row.get(16)?,
            fallback_order: row.get(17)?,
            abstention_reason: row.get(18)?,
            propensity: row.get(19)?,
            explored: row.get(20)?,
            latency_ms: row.get(21)?,
            attempts: row.get(22)?,
            error_class: row.get(23)?,
            cache_hit: row.get(24)?,
            legacy_v1: row.get(25)?,
            backend_endpoint: row.get(26)?,
            prior_decision_id: row.get(27)?,
            session_key: row.get(28)?,
        })
    }

    fn decode(self, outcomes: Vec<OutcomeView>) -> Result<DecisionView, StoreError> {
        Ok(DecisionView {
            decision_id: DecisionId::parse(&self.id).map_err(|_| {
                rusqlite::Error::InvalidColumnType(0, "decision_id".to_string(), rusqlite::types::Type::Text)
            })?,
            prior_decision_id: self
                .prior_decision_id
                .map(|id| DecisionId::parse(&id))
                .transpose()
                .map_err(|_| {
                    rusqlite::Error::InvalidColumnType(27, "prior_decision_id".to_string(), rusqlite::types::Type::Text)
                })?,
            created_at_ms: self.created_at_ms,
            has_owner: self.has_owner,
            session_key: self.session_key,
            run_id: self.run_id,
            task_id: self.task_id,
            channel: self.channel,
            enabled_snapshot: self.enabled_snapshot,
            policy_version: self.policy_version,
            scorer_version: self.scorer_version,
            backend_kind: self.backend_kind,
            backend_locality: self.backend_locality,
            configured_model: self.configured_model,
            backend_endpoint: self.backend_endpoint,
            candidates: serde_json::from_str(&self.candidates)?,
            distributions: serde_json::from_str(&self.distributions)?,
            builtin_shadow: serde_json::from_str(&self.shadow)?,
            selected_candidate_id: self.selected_candidate_id,
            directive_kind: self.directive_kind,
            fallback_order: serde_json::from_str(&self.fallback_order)?,
            abstention_reason: self.abstention_reason,
            propensity: self.propensity,
            explored: self.explored,
            latency_ms: self.latency_ms,
            attempts: serde_json::from_str(&self.attempts)?,
            error_class: self.error_class,
            cache_hit: self.cache_hit,
            legacy_v1: self.legacy_v1,
            outcomes,
        })
    }
}

fn json_len(text: Option<String>) -> u64 {
    text.and_then(|text| serde_json::from_str::<Vec<serde_json::Value>>(&text).ok())
        .map_or(0, |items| items.len() as u64)
}

fn outcomes_of(conn: &Connection, decision_id: &str) -> Result<Vec<OutcomeView>, StoreError> {
    let mut statement = conn.prepare_cached(
        "SELECT settlement_id, recorded_at_ms, classification, executed_action, executed_target, \
         provider_attempts_json, tool_calls_json, latency_ms, cost_micro_usd, reward_json, prompt_bytes_before, \
         prompt_bytes_after, tool_bytes_before, tool_bytes_after, denied_tool_attempts, completeness \
         FROM outcomes WHERE decision_id = ?1 ORDER BY recorded_at_ms, settlement_id",
    )?;
    let rows = statement.query_map([decision_id], |row| {
        let reward: Option<String> = row.get(9)?;
        Ok((
            OutcomeView {
                settlement_id: row.get(0)?,
                classification: row.get(2)?,
                executed_action: row.get(3)?,
                executed_target: row.get(4)?,
                provider_attempts: json_len(row.get(5)?),
                cost_micro_usd: row.get(8)?,
                reward: None,
                prompt_bytes_before: row.get(10)?,
                prompt_bytes_after: row.get(11)?,
                tool_bytes_before: row.get(12)?,
                tool_bytes_after: row.get(13)?,
                denied_tool_attempts: row.get(14)?,
                completeness: row.get(15)?,
            },
            reward,
        ))
    })?;
    let mut outcomes = Vec::new();
    for row in rows {
        let (mut outcome, reward) = row?;
        outcome.reward = reward.map(|reward| serde_json::from_str(&reward)).transpose()?;
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// Reads over read-only connections.
#[derive(Clone)]
pub(crate) struct DecisionStoreReader {
    path: Option<Arc<Path>>,
    /// Read without locks or side files: the disabled plane's store.
    immutable: bool,
    idle: Arc<parking_lot::Mutex<Vec<Connection>>>,
}

impl std::fmt::Debug for DecisionStoreReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionStoreReader").finish_non_exhaustive()
    }
}

/// Immutable persisted facts used by the terminal signer and verifier. The
/// digest is recomputed from actual columns, never trusted from `record_hash`.
/// Outcomes are excluded because they are appended after terminal signing.
pub(crate) struct DecisionReplayRecord {
    pub(crate) decision: DecisionView,
    pub(crate) source_event_id: Option<String>,
    pub(crate) catalog_generation: String,
    pub(crate) state_fingerprint: String,
    pub(crate) digest: String,
}

/// The largest number of idle read connections kept.
const MAX_IDLE_READERS: usize = 4;

impl DecisionStoreReader {
    fn with<T>(&self, empty: T, read: impl FnOnce(&Connection) -> Result<T, StoreError>) -> Result<T, StoreError> {
        let Some(path) = &self.path else {
            // No database exists (a disabled plane that never wrote).
            return Ok(empty);
        };
        let idle = self.idle.lock().pop();
        let conn = match idle {
            Some(conn) => conn,
            None => open_reader(path, self.immutable)?,
        };
        let result = read(&conn);
        let mut idle = self.idle.lock();
        if idle.len() < MAX_IDLE_READERS {
            idle.push(conn);
        }
        result
    }

    /// Recovery reads use one deadline for pool access, connection setup and
    /// all SQL/decoding. OS file open is not cancellable; an expired open is
    /// discarded before any SQL. The owning task must still track its return.
    fn with_until<T>(
        &self,
        deadline: Instant,
        read: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        recovery_time_left(deadline)?;
        let path = self.path.as_ref().ok_or(StoreError::RecoveryUnavailable)?;
        let idle = self
            .idle
            .try_lock_until(deadline)
            .ok_or(StoreError::RecoveryDeadline)?
            .pop();
        recovery_time_left(deadline)?;
        let conn = match idle {
            Some(conn) => conn,
            None => open_reader_connection(path, self.immutable)?,
        };
        recovery_time_left(deadline)?;
        let result = {
            let _budget = RecoveryReadBudget::new(&conn, deadline)?;
            conn.execute_batch("PRAGMA query_only = ON;")?;
            recovery_time_left(deadline)?;
            let result = read(&conn);
            recovery_time_left(deadline)?;
            result
        };
        // Returning a connection is optional and never extends the request's
        // deadline by waiting on another reader. A dropped connection is safe.
        if let Some(mut idle) = self.idle.try_lock() {
            if idle.len() < MAX_IDLE_READERS {
                idle.push(conn);
            }
        }
        recovery_time_left(deadline)?;
        result
    }

    pub(crate) fn replay_record_until(
        &self,
        scope: &DecisionScope,
        id: &DecisionId,
        deadline: Instant,
    ) -> Result<Option<DecisionReplayRecord>, StoreError> {
        recovery_scope_bounds(scope)?;
        self.with_until(deadline, |conn| {
            let transaction = conn.unchecked_transaction()?;
            let result = replay_record_on_connection_until(&transaction, scope, id, deadline)?;
            recovery_time_left(deadline)?;
            transaction.commit()?;
            Ok(result)
        })
    }

    pub(crate) fn outcome_matches_until(
        &self,
        scope: &DecisionScope,
        record: &OutcomeRecord,
        deadline: Instant,
    ) -> Result<bool, StoreError> {
        recovery_scope_bounds(scope)?;
        self.with_until(deadline, |conn| {
            let transaction = conn.unchecked_transaction()?;
            let result = outcome_matches_on_connection_until(&transaction, scope, record, deadline)?;
            recovery_time_left(deadline)?;
            transaction.commit()?;
            Ok(result)
        })
    }

    /// The decision `id` if `scope` may see it. A decision outside the scope
    /// and an unknown id give the same `None` on the same path (V6.04).
    ///
    /// # Errors
    ///
    /// A failed read or an undecodable row.
    pub(crate) fn get(&self, scope: &DecisionScope, id: &DecisionId) -> Result<Option<DecisionView>, StoreError> {
        self.with(None, |conn| {
            let (clause, [first, second]) = scope.clause();
            let columns = decision_columns(conn)?;
            let sql = format!("SELECT {columns} FROM decisions d WHERE d.decision_id = ?1 AND {clause}");
            let raw = conn
                .prepare_cached(&sql)?
                .query_row(params![id.to_string(), first, second], RawDecision::read)
                .optional()?;
            match raw {
                Some(raw) => {
                    let outcomes = outcomes_of(conn, &raw.id)?;
                    raw.decode(outcomes).map(Some)
                }
                None => Ok(None),
            }
        })
    }

    /// Read gate diagnostics only for the same authorized decision scope.
    /// Oversized payloads are unavailable to inspection, never truncated into
    /// apparently valid evidence. Callers must project a safe summary.
    pub(crate) fn inspection_gates(
        &self,
        scope: &DecisionScope,
        id: &DecisionId,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        self.with(None, |conn| {
            let (clause, [first, second]) = scope.clause();
            let sql = format!(
                "SELECT CASE WHEN length(d.gates_json) <= 1048576 THEN d.gates_json ELSE NULL END \
                 FROM decisions d WHERE d.decision_id = ?1 AND {clause}"
            );
            let raw: Option<Option<String>> = conn
                .prepare_cached(&sql)?
                .query_row(params![id.to_string(), first, second], |row| row.get(0))
                .optional()?;
            raw.flatten()
                .map(|text| serde_json::from_str(&text))
                .transpose()
                .map_err(StoreError::from)
        })
    }

    /// Read only the opt-in projection actually stored for this decision.
    /// The byte bound prevents oversized history from becoming an unbounded
    /// inspection response; absent and oversized projections stay absent.
    pub(crate) fn inspection_projection(
        &self,
        scope: &DecisionScope,
        id: &DecisionId,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        self.with(None, |conn| {
            let (clause, [first, second]) = scope.clause();
            let sql = format!(
                "SELECT CASE WHEN length(CAST(d.redacted_projection AS BLOB)) <= 1048576 \
                 THEN d.redacted_projection ELSE NULL END FROM decisions d \
                 WHERE d.decision_id = ?1 AND {clause}"
            );
            let raw: Option<Option<String>> = conn
                .prepare_cached(&sql)?
                .query_row(params![id.to_string(), first, second], |row| row.get(0))
                .optional()?;
            raw.flatten()
                .map(|text| serde_json::from_str(&text))
                .transpose()
                .map_err(StoreError::from)
        })
    }

    /// Read the decision and its replay binding in one SQLite statement. Legacy
    /// schemas cannot provide the full binding and therefore cannot be replayed.
    pub(crate) fn replay_record(
        &self,
        scope: &DecisionScope,
        id: &DecisionId,
    ) -> Result<Option<DecisionReplayRecord>, StoreError> {
        self.with(None, |conn| replay_record_on_connection(conn, scope, id))
    }

    /// A queue drain alone is not a durable acknowledgement: conflicting
    /// settlements are dropped by the writer. Compare the stored facts in the
    /// owner's scope, excluding only the first insertion time.
    pub(crate) fn outcome_matches(&self, scope: &DecisionScope, record: &OutcomeRecord) -> Result<bool, StoreError> {
        self.with(false, |conn| outcome_matches_on_connection(conn, scope, record))
    }

    /// Median total-turn latency associated with exactly one successful
    /// capability call, scoped to one owner and one catalog generation. This
    /// is an estimate, never an execution deadline or isolated tool duration.
    /// Fewer than five unambiguous complete outcomes leave the family default
    /// in force. Only the newest 1024 scoped decisions are joined, and at
    /// most 1024 outcome rows are decoded.
    pub(crate) fn capability_latency_p50(
        &self,
        workspace_id: &str,
        owner_id: &str,
        catalog_generation: &str,
    ) -> Result<BTreeMap<String, u32>, StoreError> {
        self.with(BTreeMap::new(), |conn| {
            // Disabled/keyless services may read an older store immutably.
            // Its scope/time index lacks the tie breaker needed for a
            // bounded read, so keep the versioned family defaults.
            if store_schema::version(conn)? < 4 {
                return Ok(BTreeMap::new());
            }
            // The existing scope/time index bounds the history read before
            // joining outcomes. A stale generation can only cause fallback.
            let mut statement = conn.prepare_cached(CAPABILITY_LATENCY_SQL)?;
            let mut rows = statement.query(params![workspace_id, owner_id, catalog_generation])?;
            let mut samples: BTreeMap<String, Vec<u32>> = BTreeMap::new();
            while let Some(row) = rows.next()? {
                let ids_json: Option<String> = row.get(0)?;
                let calls_json: Option<String> = row.get(1)?;
                let target: Option<String> = row.get(2)?;
                let latency: i64 = row.get(3)?;
                let (Some(ids_json), Some(calls_json), Some(target), Ok(latency)) =
                    (ids_json, calls_json, target, u32::try_from(latency))
                else {
                    continue;
                };
                if ids_json.len() > MAX_OUTCOME_STORED_BYTES || calls_json.len() > MAX_OUTCOME_STORED_BYTES {
                    return Err(StoreError::Unsafe("outcome latency sample is oversized".to_owned()));
                }
                let ids: Vec<String> = serde_json::from_str(&ids_json)?;
                let calls: Vec<RecordedToolCall> = serde_json::from_str(&calls_json)?;
                let ([id], [call]) = (ids.as_slice(), calls.as_slice()) else {
                    continue;
                };
                if !bounded_capability_id(id) || call.capability_id != *id || call.status != "success" {
                    continue;
                }
                if id.split(':').next() != Some(target.as_str()) {
                    continue;
                }
                samples.entry(id.clone()).or_default().push(latency);
            }
            Ok(samples
                .into_iter()
                .filter_map(|(id, mut values)| {
                    if values.len() < 5 {
                        return None;
                    }
                    values.sort_unstable();
                    // Upper median for an even count avoids underestimating
                    // the observed midpoint at a tight latency gate.
                    values.get(values.len() / 2).copied().map(|median| (id, median))
                })
                .collect())
        })
    }

    /// The non-legacy decisions of `scope` created in `window`, with their
    /// outcomes, and the number of legacy rows left out.
    ///
    /// # Errors
    ///
    /// A failed read or an undecodable row.
    pub(crate) fn evaluation_rows(
        &self,
        scope: &DecisionScope,
        window: TimeWindow,
    ) -> Result<EvaluationRows, StoreError> {
        self.with(EvaluationRows::default(), |conn| {
            let (clause, [first, second]) = scope.clause();
            let columns = decision_columns(conn)?;
            let sql = format!(
                "SELECT {columns} FROM decisions d WHERE ?1 = ?1 AND {clause} \
                 AND d.created_at_ms >= ?4 AND d.created_at_ms < ?5 ORDER BY d.created_at_ms, d.decision_id"
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let raws = statement
                .query_map(
                    params!["", first, second, window.from_ms, window.to_ms],
                    RawDecision::read,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut rows = EvaluationRows::default();
            for raw in raws {
                if raw.legacy_v1 {
                    rows.legacy_excluded += 1;
                    continue;
                }
                let outcomes = outcomes_of(conn, &raw.id)?;
                rows.decisions.push(raw.decode(outcomes)?);
            }
            Ok(rows)
        })
    }

    /// The decisions of `scope` in `window`, newest first, at most `limit`
    /// (the Gateway's list, deferred from CDP-09 by D-090). Imported v1 rows
    /// have no owner, so an owner scope never lists them.
    ///
    /// # Errors
    ///
    /// A failed read.
    pub(crate) fn list(
        &self,
        scope: &DecisionScope,
        window: TimeWindow,
        limit: u32,
    ) -> Result<Vec<DecisionView>, StoreError> {
        self.with(Vec::new(), |conn| {
            let (clause, [first, second]) = scope.clause();
            let columns = decision_columns(conn)?;
            let sql = format!(
                "SELECT {columns} FROM decisions d WHERE ?1 = ?1 AND {clause} \
                 AND d.created_at_ms >= ?4 AND d.created_at_ms < ?5 \
                 ORDER BY d.created_at_ms DESC, d.decision_id DESC LIMIT ?6"
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let raws = statement
                .query_map(
                    params!["", first, second, window.from_ms, window.to_ms, i64::from(limit)],
                    RawDecision::read,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut views = Vec::with_capacity(raws.len());
            for raw in raws {
                let outcomes = outcomes_of(conn, &raw.id)?;
                views.push(raw.decode(outcomes)?);
            }
            Ok(views)
        })
    }

    /// The recorded import progress of the v1 file `file`.
    ///
    /// # Errors
    ///
    /// A failed read.
    pub(crate) fn import_progress(&self, file: &str) -> Result<Option<ImportProgress>, StoreError> {
        self.with(None, |conn| {
            let value: Option<String> = conn
                .prepare_cached("SELECT value FROM store_meta WHERE key = ?1")?
                .query_row([ImportProgress::meta_key(file)], |row| row.get(0))
                .optional()?;
            Ok(value.and_then(|value| {
                let (digest, lines) = value.split_once(' ')?;
                Some(ImportProgress {
                    file: file.to_string(),
                    digest: digest.to_string(),
                    lines: lines.parse().ok()?,
                })
            }))
        })
    }

    /// Store facts for `prx causal status`: the schema version and the
    /// number of decisions and outcomes.
    ///
    /// # Errors
    ///
    /// A failed read.
    pub(crate) fn counts(&self) -> Result<StoreCounts, StoreError> {
        self.with(StoreCounts::default(), |conn| {
            Ok(StoreCounts {
                schema_version: store_schema::version(conn)?,
                decisions: conn.query_row("SELECT count(*) FROM decisions", [], |row| row.get(0))?,
                legacy_decisions: conn.query_row("SELECT count(*) FROM decisions WHERE legacy_v1 = 1", [], |row| {
                    row.get(0)
                })?,
                outcomes: conn.query_row("SELECT count(*) FROM outcomes", [], |row| row.get(0))?,
            })
        })
    }
}

/// Row counts of the store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StoreCounts {
    pub(crate) schema_version: i64,
    pub(crate) decisions: i64,
    pub(crate) legacy_decisions: i64,
    pub(crate) outcomes: i64,
}

/// A read-only connection that never follows a symlink. An `immutable` one
/// takes no lock and creates no `-wal` or `-shm` file: the disabled plane's
/// reads leave the directory as it was (D-091 F-5).
fn open_reader_connection(path: &Path, immutable: bool) -> Result<Connection, StoreError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let conn = if immutable {
        Connection::open_with_flags(
            format!("file:{}?mode=ro&immutable=1", uri_path(path)?),
            flags | OpenFlags::SQLITE_OPEN_URI,
        )?
    } else {
        Connection::open_with_flags(path, flags)?
    };
    Ok(conn)
}

fn open_reader(path: &Path, immutable: bool) -> Result<Connection, StoreError> {
    let conn = open_reader_connection(path, immutable)?;
    conn.execute_batch("PRAGMA query_only = ON;")?;
    crate::runtime::sqlite_pool::install_bounded_busy_handler(&conn)?;
    Ok(conn)
}

/// `path` for an SQLite `file:` URI: `%`, `?` and `#` escaped.
fn uri_path(path: &Path) -> Result<String, StoreError> {
    let text = path
        .to_str()
        .ok_or_else(|| StoreError::Unsafe("the store path is not valid UTF-8".to_string()))?;
    Ok(text.replace('%', "%25").replace('?', "%3f").replace('#', "%23"))
}

/// The per-process registry: one store, and so one writer thread, per
/// database.
static STORES: LazyLock<parking_lot::Mutex<HashMap<PathBuf, Weak<CausalDecisionStore>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// The causal decision store of one workspace.
pub(crate) struct CausalDecisionStore {
    path: PathBuf,
    mode: StoreMode,
    /// The store's stable id; `None` only when no database exists yet.
    workspace_id: Option<Arc<str>>,
    reader: DecisionStoreReader,
    writer: Option<DecisionStoreWriter>,
}

impl std::fmt::Debug for CausalDecisionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CausalDecisionStore")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// How the writer runs; tests shrink the queue and hold the thread.
struct WriterSetup {
    capacity: usize,
    #[cfg(test)]
    hold: Option<Arc<parking_lot::Mutex<()>>>,
}

impl Default for WriterSetup {
    fn default() -> Self {
        Self {
            capacity: QUEUE_CAPACITY,
            #[cfg(test)]
            hold: None,
        }
    }
}

impl CausalDecisionStore {
    /// Open an independent read-only view. Disabled or keyless decision
    /// services must never inherit a process-shared writable handle.
    pub(crate) fn open_unshared_read_only(workspace_dir: &Path) -> Arc<Self> {
        let store = Self::open_read_only(workspace_dir, true).unwrap_or_else(|error| {
            // A disabled or keyless decision must still return a builtin
            // directive. Refuse the unsafe database and use an empty view;
            // this fallback never opens or changes a file.
            tracing::warn!(%error, "decision store read-only view unavailable; using empty view");
            Self {
                path: db_path(workspace_dir),
                mode: StoreMode::ReadOnly,
                workspace_id: None,
                reader: DecisionStoreReader {
                    path: None,
                    immutable: true,
                    idle: Arc::default(),
                },
                writer: None,
            }
        });
        Arc::new(store)
    }

    /// Borrow an existing writable store without creating or upgrading it.
    pub(crate) fn existing_shared_writer(workspace_dir: &Path) -> Option<Arc<Self>> {
        // Recovery may borrow an already owned writer, but must not create,
        // migrate, repair or upgrade storage merely to replay a terminal.
        STORES
            .lock()
            .get(&db_path(workspace_dir))
            .and_then(Weak::upgrade)
            .filter(|store| store.mode == StoreMode::ReadWrite && store.writer.is_some())
    }

    /// The store of `workspace_dir`, shared by the process: a second open of
    /// the same database returns the first store while it lives. A
    /// read-write request replaces a read-only store.
    ///
    /// With [`StoreMode::ReadOnly`] nothing is created or changed; a missing
    /// database reads as empty. With [`StoreMode::ReadWrite`] the database
    /// is created or migrated, a corrupt one is quarantined and replaced,
    /// the retention horizon is applied, and a newer schema leaves the store
    /// read-only.
    ///
    /// # Errors
    ///
    /// [`StoreError`] when the database cannot be opened even after
    /// recovery. Callers carry on without a store.
    pub(crate) fn open_shared(
        workspace_dir: &Path,
        retention: Retention,
        events: Arc<dyn CausalEventSink>,
        mode: StoreMode,
    ) -> Result<Arc<Self>, StoreError> {
        let path = db_path(workspace_dir);
        let mut stores = STORES.lock();
        if let Some(existing) = stores.get(&path).and_then(Weak::upgrade) {
            if existing.mode == StoreMode::ReadWrite || mode == StoreMode::ReadOnly {
                return Ok(existing);
            }
        }
        let store = Arc::new(Self::open(
            workspace_dir,
            retention,
            events,
            mode,
            WriterSetup::default(),
        )?);
        stores.retain(|_, store| store.strong_count() > 0);
        stores.insert(path, Arc::downgrade(&store));
        Ok(store)
    }

    fn open(
        workspace_dir: &Path,
        retention: Retention,
        events: Arc<dyn CausalEventSink>,
        mode: StoreMode,
        setup: WriterSetup,
    ) -> Result<Self, StoreError> {
        if mode == StoreMode::ReadOnly {
            return Self::open_read_only(workspace_dir, true);
        }
        let dir = crate::decision::key::prepare_decision_dir(workspace_dir).map_err(StoreError::Unsafe)?;
        let path = real_path(&dir)?;
        check_files(&path)?;
        // A database written by a newer build is recognised without ever
        // opening it for writing (D-091 F-17).
        if path.exists() {
            match probe_version(&path) {
                Ok(version) if version > store_schema::SCHEMA_VERSION => {
                    tracing::warn!(
                        version,
                        supported = store_schema::SCHEMA_VERSION,
                        "the decision store was written by a newer PRX; it is read-only until that version runs \
                         again"
                    );
                    return Self::open_read_only(workspace_dir, false);
                }
                Ok(_) => {}
                Err(error) if is_corruption(&error) => recover(&path, events.as_ref())?,
                Err(error) => return Err(error),
            }
        }
        let (mut conn, state) = match open_writer(&path) {
            Ok(opened) => opened,
            Err(error) if is_corruption(&error) => {
                recover(&path, events.as_ref())?;
                open_writer(&path)?
            }
            Err(error) => return Err(error),
        };
        restrict_files(&path)?;
        let reader = DecisionStoreReader {
            path: Some(Arc::from(path.as_path())),
            immutable: false,
            idle: Arc::default(),
        };
        if let SchemaState::Newer(version) = state {
            // Another process upgraded it between the probe and the open.
            tracing::warn!(
                version,
                "the decision store was upgraded by a newer PRX while opening; it is read-only"
            );
            drop(conn);
            return Self::open_read_only(workspace_dir, false);
        }
        let workspace_id = store_schema::workspace_id(&conn)?.map_or_else(
            || Err(StoreError::Unsafe("the store has no workspace id".to_string())),
            |id| Ok(Arc::<str>::from(id)),
        )?;
        if let Some(cutoff) = retention.cutoff(now_ms()) {
            prune(&mut conn, cutoff)?;
        }
        let (queue, commands) = std::sync::mpsc::sync_channel(setup.capacity);
        let thread_events = Arc::clone(&events);
        let thread_path = path.clone();
        std::thread::Builder::new()
            .name("prx-decision-store".to_string())
            .spawn(move || {
                run_writer(WriterThread {
                    conn,
                    commands,
                    events: thread_events,
                    retention,
                    path: thread_path,
                    writes_since_prune: 0,
                    #[cfg(test)]
                    hold: setup.hold,
                });
            })?;
        Ok(Self {
            path,
            mode,
            workspace_id: Some(Arc::clone(&workspace_id)),
            reader,
            writer: Some(DecisionStoreWriter {
                queue,
                events,
                workspace_id,
            }),
        })
    }

    /// A read-only store over `path`. Nothing is created: a missing
    /// database reads as empty. `immutable` reads take no lock and create no
    /// side file (the disabled plane).
    fn open_read_only(workspace_dir: &Path, immutable: bool) -> Result<Self, StoreError> {
        let dir_exists = crate::decision::key::check_decision_dir(workspace_dir).map_err(StoreError::Unsafe)?;
        let path = if dir_exists {
            real_path(&workspace_dir.join("self").join("causal_tree"))?
        } else {
            db_path(workspace_dir)
        };
        let exists = dir_exists && check_files(&path)?;
        // An immutable read ignores the WAL: when a non-empty -wal and its
        // -shm already exist (a writer that has not checkpointed), a plain
        // read-only open sees those rows and creates no file (D-093 N-2).
        let live_wal = std::fs::symlink_metadata(side_file(&path, "-wal")).is_ok_and(|wal| wal.len() > 0)
            && side_file(&path, "-shm").exists();
        let reader = DecisionStoreReader {
            path: exists.then(|| Arc::from(path.as_path())),
            immutable: immutable && !live_wal,
            idle: Arc::default(),
        };
        let workspace_id = if exists {
            reader
                .with(None, |conn| match store_schema::workspace_id(conn) {
                    Ok(id) => Ok(id),
                    Err(error) => {
                        tracing::warn!("the decision store's workspace id could not be read: {error}");
                        Ok(None)
                    }
                })?
                .map(Arc::<str>::from)
        } else {
            None
        };
        Ok(Self {
            path,
            mode: StoreMode::ReadOnly,
            workspace_id,
            reader,
            writer: None,
        })
    }

    /// Close `store` for this holder: everything queued is written, and when
    /// no other holder in the process shares it, the idle readers are closed
    /// and the writer checkpoints the WAL and closes its connection, so no
    /// `-wal` or `-shm` file is left (R-I1-phase2 R-1). Waits until
    /// `deadline` at most.
    ///
    /// The other holders are counted under the registry lock, and the last
    /// holder removes the store from the registry in the same critical
    /// section, so no concurrent [`Self::open_shared`] can pick up a store
    /// that is closing: it opens a fresh one (R-I1-confirm Note 1). A holder
    /// keeps the `Arc<CausalDecisionStore>`, never only a writer clone: a
    /// writer clone is not counted (the CDP-22 contract).
    pub(crate) async fn close(store: Arc<Self>, deadline: Instant) {
        let Some(writer) = store.writer() else {
            return;
        };
        writer.drain(deadline).await;
        {
            let mut stores = STORES.lock();
            if Arc::strong_count(&store) > 1 {
                return;
            }
            stores.retain(|_, held| !std::ptr::eq(held.as_ptr(), Arc::as_ptr(&store)));
        }
        store.reader.idle.lock().clear();
        writer.close(deadline).await;
    }

    /// The writer; `None` when the store is read-only (a disabled plane, or
    /// a database written by a newer build).
    pub(crate) fn writer(&self) -> Option<DecisionStoreWriter> {
        self.writer.clone()
    }

    /// The reader.
    pub(crate) fn reader(&self) -> DecisionStoreReader {
        self.reader.clone()
    }

    /// Whether the store may write.
    pub(crate) const fn mode(&self) -> StoreMode {
        self.mode
    }

    /// The database path, for status output.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The store's stable workspace id, the one scope key; `None` when no
    /// database exists yet.
    pub(crate) fn workspace_id(&self) -> Option<Arc<str>> {
        self.workspace_id.clone()
    }
}

fn db_path(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("self").join("causal_tree").join(DB_FILE)
}

/// The database path under the checked store directory `dir`, with the
/// directory's ancestors resolved: SQLite's `NOFOLLOW` refuses a symlink
/// anywhere in the path (a system alias such as `/var` on macOS), while the
/// store's own directories were checked to be plain directories.
fn real_path(dir: &Path) -> Result<PathBuf, StoreError> {
    Ok(std::fs::canonicalize(dir)?.join(DB_FILE))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The schema version of the database at `path`, read through a read-only
/// connection (a corrupt file or not-a-database fails here).
fn probe_version(path: &Path) -> Result<i64, StoreError> {
    let conn = open_reader(path, false)?;
    let version = store_schema::version(&conn)?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get::<_, i64>(0))?;
    Ok(version)
}

/// Open the writer connection without following a symlink, check that the
/// file is a database, switch it to WAL and bring its schema up to date in
/// an IMMEDIATE transaction (D-091 F-2).
fn open_writer(path: &Path) -> Result<(Connection, SchemaState), StoreError> {
    let mut conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    crate::runtime::sqlite_pool::install_bounded_busy_handler(&conn)?;
    // Reading the schema touches the file header and the schema table: a
    // corrupt file or not-a-database fails here.
    let version = store_schema::version(&conn)?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get::<_, i64>(0))?;
    if version > store_schema::SCHEMA_VERSION {
        return Ok((conn, SchemaState::Newer(version)));
    }
    enable_wal(&conn)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    let state = store_schema::migrate(&mut conn)?;
    Ok((conn, state))
}

/// The longest a first open waits for a concurrent one's switch to WAL.
const WAL_SWITCH_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Switch `conn`'s database to WAL. The switch upgrades the pragma's read
/// to a write, and SQLite does not call the busy handler for that upgrade
/// (it avoids a deadlock), so a concurrent first open gets `SQLITE_BUSY` at
/// once. It is retried within [`WAL_SWITCH_WAIT`]; once another open has
/// switched the file, the pragma answers `wal` at once.
fn enable_wal(conn: &Connection) -> Result<(), StoreError> {
    let deadline = Instant::now() + WAL_SWITCH_WAIT;
    let mut pause = std::time::Duration::from_millis(2);
    loop {
        match conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get::<_, String>(0)) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(mode) => {
                return Err(StoreError::Unsafe(format!(
                    "the decision store could not switch to WAL (journal mode {mode})"
                )));
            }
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy && Instant::now() < deadline =>
            {
                std::thread::sleep(pause);
                pause = (pause * 2).min(std::time::Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Move a corrupt database aside and report the recovery.
fn recover(path: &Path, events: &dyn CausalEventSink) -> Result<(), StoreError> {
    quarantine(path)?;
    tracing::warn!(
        "the decision store database was corrupt or unreadable; it was moved aside and a fresh store was created"
    );
    emit_incomplete(events, CausalLineage::default(), IncompleteReason::StoreRecovered);
    Ok(())
}

/// Whether the database exists, refusing a symlinked or special database
/// or side file (D-091 F-1).
fn check_files(path: &Path) -> Result<bool, StoreError> {
    let mut exists = false;
    for (file, database) in [
        (path.to_path_buf(), true),
        (side_file(path, "-wal"), false),
        (side_file(path, "-shm"), false),
    ] {
        match std::fs::symlink_metadata(&file) {
            Ok(metadata) if metadata.file_type().is_file() => exists |= database,
            Ok(_) => {
                return Err(StoreError::Unsafe(format!(
                    "{} is not a regular file",
                    file.file_name()
                        .map_or_else(String::new, |name| name.to_string_lossy().to_string())
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(StoreError::Io(error)),
        }
    }
    Ok(exists)
}

const fn is_corruption(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
            if matches!(failure.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}

/// Move a corrupt database and its side files aside (`0600`) under a fresh
/// name that is never overwritten: each is linked to its new name, which
/// fails when that name exists, then unlinked (D-091 F-12). Only regular
/// files are moved ([`check_files`] ran first).
fn quarantine(path: &Path) -> Result<(), StoreError> {
    let stamp = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ"),
        uuid::Uuid::new_v4().simple()
    );
    quarantine_as(path, &stamp)
}

/// [`quarantine`] under `stamp`: a name that is taken already is an error,
/// never overwritten.
fn quarantine_as(path: &Path, stamp: &str) -> Result<(), StoreError> {
    check_files(path)?;
    // The side files first: a failure part way never leaves a WAL beside a
    // fresh database (D-093 N-5).
    for suffix in ["-wal", "-shm", ""] {
        let from = side_file(path, suffix);
        if !from.exists() {
            continue;
        }
        let aside = path.with_file_name(format!("{DB_FILE}.corrupt-{stamp}{suffix}"));
        std::fs::hard_link(&from, &aside)?;
        std::fs::remove_file(&from)?;
        restrict(&aside, 0o600)?;
    }
    Ok(())
}

fn side_file(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Narrow the database and its side files to `0600`.
fn restrict_files(path: &Path) -> Result<(), StoreError> {
    restrict(path, 0o600)?;
    for suffix in ["-wal", "-shm"] {
        let side = side_file(path, suffix);
        if side.exists() {
            restrict(&side, 0o600)?;
        }
    }
    Ok(())
}

/// Narrow a regular file to `mode`; a symlink is refused, never followed.
#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(StoreError::Unsafe("a store file is not a regular file".to_string()));
    }
    if metadata.permissions().mode() & 0o777 != mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<(), StoreError> {
    Ok(())
}

/// Whether every decision of `ids` is stored.
fn all_stored(conn: &Connection, ids: &[DecisionId]) -> Result<bool, StoreError> {
    let mut statement = conn.prepare_cached("SELECT 1 FROM decisions WHERE decision_id = ?1")?;
    for id in ids {
        if !statement.exists([id.to_string()])? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Delete decisions created before `cutoff_ms`, [`PRUNE_BATCH`] per
/// transaction; their outcomes cascade. Returns the number deleted.
fn prune(conn: &mut Connection, cutoff_ms: i64) -> Result<u64, StoreError> {
    let mut deleted = 0_u64;
    loop {
        let transaction = conn.transaction()?;
        let batch = transaction.execute(
            "DELETE FROM decisions WHERE rowid IN \
             (SELECT rowid FROM decisions WHERE created_at_ms < ?1 LIMIT ?2)",
            params![cutoff_ms, PRUNE_BATCH],
        )?;
        transaction.commit()?;
        deleted += batch as u64;
        if i64::try_from(batch).unwrap_or(i64::MAX) < PRUNE_BATCH {
            return Ok(deleted);
        }
    }
}

/// What inserting a decision found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inserted {
    Recorded,
    AlreadyRecorded,
    Conflict,
}

fn insert_decision(conn: &Connection, record: &DecisionRecord) -> Result<Inserted, StoreError> {
    let encoded = EncodedDecision::of(record)?;
    const SQL: &str = "INSERT OR IGNORE INTO decisions (decision_id, created_at_ms, workspace_id, prior_decision_id, owner_id, \
        session_key, run_id, task_id, source_event_id, causation_id, channel, enabled_snapshot, state_fingerprint, \
        redacted_projection, protocol_version, policy_version, catalog_generation, scorer_version, backend_kind, \
        backend_locality, configured_model, backend_endpoint, claimed_model, provider_request_id, candidates_json, \
        gates_json, distributions_json, builtin_shadow_json, features_json, selected_candidate_id, directive_kind, \
        fallback_order_json, abstention_reason, propensity, explored, latency_ms, usage_json, attempts_json, \
        error_class, cache_hit, response_sha256, legacy_v1, record_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, \
        ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, \
        ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43)";
    let values = encoded
        .columns
        .iter()
        .map(|(_, value)| value.as_value_ref())
        .chain(std::iter::once(rusqlite::types::ValueRef::Text(
            encoded.record_hash.as_bytes(),
        )))
        .map(rusqlite::types::ToSqlOutput::Borrowed);
    let changed = conn.execute(SQL, rusqlite::params_from_iter(values))?;
    if changed == 1 {
        return Ok(Inserted::Recorded);
    }
    let existing: Option<(String, Option<String>)> = conn
        .prepare_cached("SELECT record_hash, prior_decision_id FROM decisions WHERE decision_id = ?1")?
        .query_row([record.decision_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let prior = record.lineage.prior_decision_id.map(|id| id.to_string());
    Ok(
        if existing
            .as_ref()
            .is_some_and(|(hash, stored_prior)| hash == &encoded.record_hash && stored_prior == &prior)
        {
            Inserted::AlreadyRecorded
        } else {
            Inserted::Conflict
        },
    )
}

/// Commit the whole chain, or none of it. A duplicate with different content
/// aborts the transaction, including earlier rows newly inserted by it.
fn insert_decision_batch(
    conn: &mut Connection,
    records: &[DecisionRecord],
) -> Result<Option<Vec<Inserted>>, StoreError> {
    let transaction = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut inserted = Vec::with_capacity(records.len());
    for record in records {
        match insert_decision(&transaction, record)? {
            Inserted::Conflict => return Ok(None),
            status => inserted.push(status),
        }
    }
    transaction.commit()?;
    Ok(Some(inserted))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutcomeInserted {
    Recorded,
    AlreadyRecorded,
    Conflict,
    UnknownDecision,
}

fn insert_outcome(conn: &mut Connection, record: &OutcomeRecord) -> Result<OutcomeInserted, StoreError> {
    let transaction = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let decision_id = record.decision_id.to_string();
    let known = transaction
        .prepare_cached("SELECT 1 FROM decisions WHERE decision_id = ?1")?
        .query_row([decision_id.as_str()], |_| Ok(()))
        .optional()?
        .is_some();
    if !known {
        return Ok(OutcomeInserted::UnknownDecision);
    }
    let encoded = EncodedOutcome::of(record)?;
    let names = encoded
        .columns
        .iter()
        .map(|(name, _)| *name)
        .chain(std::iter::once("record_hash"))
        .collect::<Vec<_>>();
    let placeholders = (1..=names.len()).map(|index| format!("?{index}")).collect::<Vec<_>>();
    let sql = format!(
        "INSERT OR IGNORE INTO outcomes ({}) VALUES ({})",
        names.join(", "),
        placeholders.join(", ")
    );
    let values = encoded
        .columns
        .iter()
        .map(|(_, value)| value.clone())
        .chain(std::iter::once(rusqlite::types::Value::Text(
            encoded.record_hash.clone(),
        )));
    if transaction.execute(&sql, rusqlite::params_from_iter(values))? == 1 {
        transaction.commit()?;
        return Ok(OutcomeInserted::Recorded);
    }
    let existing: Option<String> = transaction
        .prepare_cached("SELECT record_hash FROM outcomes WHERE decision_id = ?1 AND settlement_id = ?2")?
        .query_row(params![decision_id, record.settlement_id], |row| row.get(0))
        .optional()?;
    let status = if existing.as_deref() == Some(encoded.record_hash.as_str()) {
        OutcomeInserted::AlreadyRecorded
    } else {
        OutcomeInserted::Conflict
    };
    transaction.commit()?;
    Ok(status)
}

fn outcome_lineage(conn: &Connection, decision_id: DecisionId) -> Result<CausalLineage, StoreError> {
    let row = conn
        .prepare_cached(
            "SELECT workspace_id, owner_id, session_key, run_id, task_id, source_event_id, causation_id \
             FROM decisions WHERE decision_id = ?1",
        )?
        .query_row([decision_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .optional()?;
    let Some((workspace, owner, session, run, task, source, causation)) = row else {
        return Ok(CausalLineage {
            decision_id: Some(Arc::from(decision_id.to_string())),
            ..CausalLineage::default()
        });
    };
    Ok(CausalLineage {
        workspace_id: Arc::from(workspace),
        owner_id: owner.map(Arc::from),
        session_key: session.map(Arc::from),
        run_id: run.map(Arc::from),
        task_id: task.map(Arc::from),
        source_event_id: source.map(Arc::from),
        causation_event_id: causation.map(Arc::from),
        decision_id: Some(Arc::from(decision_id.to_string())),
    })
}

fn save_progress(conn: &Connection, progress: &ImportProgress) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO store_meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![ImportProgress::meta_key(&progress.file), progress.meta_value()],
    )
}

struct WriterThread {
    conn: Connection,
    commands: Receiver<WriterMessage>,
    events: Arc<dyn CausalEventSink>,
    retention: Retention,
    path: PathBuf,
    writes_since_prune: u32,
    #[cfg(test)]
    hold: Option<Arc<parking_lot::Mutex<()>>>,
}

fn run_writer(mut thread: WriterThread) {
    #[cfg(test)]
    let hold = thread.hold.take();
    while let Ok(command) = thread.commands.recv() {
        #[cfg(test)]
        let _held = hold.as_ref().map(|hold| hold.lock());
        match command {
            WriterMessage::DecisionBatch(records) => thread.write_decisions(&records),
            WriterMessage::Outcome(record) => thread.write_outcome(&record),
            WriterMessage::ChildOutcome(record) => thread.write_child_outcome(&record),
            WriterMessage::Imported(progress, imported) => match all_stored(&thread.conn, &imported) {
                Ok(true) => {
                    if let Err(error) = save_progress(&thread.conn, &progress) {
                        tracing::warn!("decision store could not record v1 import progress: {error}");
                    }
                }
                Ok(false) => {
                    tracing::warn!("a v1 feedback file was not fully stored; it is imported again on the next open");
                }
                Err(error) => {
                    tracing::warn!("decision store could not check v1 import progress: {error}")
                }
            },
            WriterMessage::Drain(done) => {
                let _ = done.send(());
            }
            WriterMessage::Close(done) => {
                thread.close();
                let _ = done.send(());
                return;
            }
        }
    }
}

impl WriterThread {
    /// Checkpoint the WAL into the database and close the connection. The
    /// last connection to close removes `-wal` and `-shm`, so an enabled run
    /// leaves no pending WAL behind (R-I1-phase2 R-1).
    fn close(self) {
        if let Err(error) = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get::<_, i64>(0))
        {
            tracing::warn!("decision store could not checkpoint its WAL on close: {error}");
        }
        if let Err((_, error)) = self.conn.close() {
            tracing::warn!("decision store connection did not close cleanly: {error}");
        }
    }

    fn write_decisions(&mut self, records: &[DecisionRecord]) {
        match insert_decision_batch(&mut self.conn, records) {
            Ok(Some(inserted)) => {
                for (record, status) in records.iter().zip(inserted) {
                    if status == Inserted::Recorded {
                        self.events.emit(CausalEvent {
                            kind: CausalEventKind::DecisionCommitted,
                            lineage: lineage_of(record),
                            fields: CausalEventFields::default(),
                        });
                    }
                }
            }
            Ok(None) => {
                tracing::warn!("a conflicting decision chain was refused; no new rows from that chain were kept");
                for record in records {
                    emit_incomplete(self.events.as_ref(), lineage_of(record), IncompleteReason::Conflict);
                }
            }
            Err(error) => {
                tracing::warn!("decision store chain write failed: {error}");
                for record in records {
                    emit_incomplete(self.events.as_ref(), lineage_of(record), IncompleteReason::StoreFailed);
                }
            }
        }
        self.writes_since_prune = self
            .writes_since_prune
            .saturating_add(u32::try_from(records.len()).unwrap_or(u32::MAX));
        if self.writes_since_prune >= PRUNE_EVERY {
            self.writes_since_prune = 0;
            if let Some(cutoff) = self.retention.cutoff(now_ms()) {
                if let Err(error) = prune(&mut self.conn, cutoff) {
                    tracing::warn!("decision store retention failed: {error}");
                }
            }
            if let Err(error) = restrict_files(&self.path) {
                tracing::warn!("decision store permissions could not be narrowed: {error}");
            }
        }
    }

    fn write_outcome(&mut self, record: &OutcomeRecord) {
        let lineage = outcome_lineage(&self.conn, record.decision_id).unwrap_or_else(|_| CausalLineage {
            decision_id: Some(Arc::from(record.decision_id.to_string())),
            ..CausalLineage::default()
        });
        match insert_outcome(&mut self.conn, record) {
            Ok(OutcomeInserted::Recorded) => {
                let created_at_ms = self
                    .conn
                    .query_row(
                        "SELECT created_at_ms FROM decisions WHERE decision_id = ?1",
                        [record.decision_id.to_string()],
                        |row| row.get::<_, i64>(0),
                    )
                    .ok();
                let delay_ms = created_at_ms
                    .and_then(|created| record.recorded_at_ms.checked_sub(created))
                    .and_then(|delay| u64::try_from(delay).ok());
                self.events.emit(CausalEvent {
                    kind: CausalEventKind::OutcomeRecorded,
                    lineage,
                    fields: CausalEventFields {
                        latency_ms: delay_ms,
                        ..CausalEventFields::default()
                    },
                });
            }
            Ok(OutcomeInserted::AlreadyRecorded) => {}
            Ok(OutcomeInserted::Conflict) => {
                tracing::warn!("a conflicting terminal settlement was refused");
                emit_incomplete(self.events.as_ref(), lineage, IncompleteReason::Conflict);
            }
            Ok(OutcomeInserted::UnknownDecision) => {
                tracing::warn!("terminal settlement named an unknown decision");
                emit_incomplete(self.events.as_ref(), lineage, IncompleteReason::UnknownDecision);
            }
            Err(error) => {
                tracing::warn!("decision store outcome write failed: {error}");
                emit_incomplete(self.events.as_ref(), lineage, IncompleteReason::StoreFailed);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Fixtures for the store's and evaluation's tests: records, and
    //! outcomes written straight to the database (the outcome writer is
    //! CDP-22's).

    use super::*;

    /// A synthetic complete single-call settlement for scoped latency-reader
    /// integration tests. Production outcomes use the bounded writer.
    pub(crate) fn write_single_call_latency(
        workspace_dir: &Path,
        decision_id: DecisionId,
        settlement_id: &str,
        capability_id: &str,
        latency_ms: u32,
    ) {
        let conn = Connection::open(db_path(workspace_dir)).expect("test: open store");
        let family = capability_id.split(':').next().expect("test: family");
        let ids = serde_json::to_string(&[capability_id]).expect("test: ids");
        let calls = serde_json::to_string(&[RecordedToolCall {
            capability_id: capability_id.to_owned(),
            status: "success".to_owned(),
        }])
        .expect("test: calls");
        conn.execute(
            "INSERT INTO outcomes (decision_id, settlement_id, recorded_at_ms, classification,
             executed_action, executed_target, tool_calls_json, capability_ids_json, latency_ms,
             completeness, record_hash) VALUES (?1, ?2, ?3, 'success', 'use_tool', ?4, ?5, ?6, ?7,
             'complete', 'synthetic-test')",
            params![
                decision_id.to_string(),
                settlement_id,
                now_ms(),
                family,
                calls,
                ids,
                i64::from(latency_ms)
            ],
        )
        .expect("test: committed latency outcome");
    }

    /// A complete decision record for `id` in `workspace`, owned by `owner`.
    pub(crate) fn record(id: DecisionId, workspace: &str, owner: Option<&str>, created_at_ms: i64) -> DecisionRecord {
        DecisionRecord {
            decision_id: id,
            created_at_ms,
            lineage: RecordLineage {
                workspace_id: Arc::from(workspace),
                owner_id: owner.map(Arc::from),
                run_id: Some(Arc::from("run-1")),
                ..RecordLineage::default()
            },
            enabled_snapshot: true,
            state_fingerprint: "f".repeat(64),
            redacted_projection: None,
            protocol_version: 1,
            policy_version: "policy-v1".to_string(),
            catalog_generation: "catalog-1".to_string(),
            scorer_version: "scorer-v1".to_string(),
            backend_kind: "builtin".to_string(),
            backend_locality: "builtin".to_string(),
            configured_model: "builtin-v1".to_string(),
            backend_endpoint: None,
            claimed_model: None,
            provider_request_id: None,
            candidates: vec![
                RecordedCandidate {
                    id: "c-answer".to_string(),
                    action: "answer".to_string(),
                    target: "none".to_string(),
                    propensity: None,
                },
                RecordedCandidate {
                    id: "c-retrieve".to_string(),
                    action: "retrieve".to_string(),
                    target: "memory".to_string(),
                    propensity: None,
                },
            ],
            gates: serde_json::json!([]),
            distributions: RecordedDistributions::default(),
            builtin_shadow: RecordedShadow::default(),
            features: serde_json::json!({}),
            selected_candidate_id: Some("c-answer".to_string()),
            directive_kind: "answer".to_string(),
            fallback_order: vec!["builtin".to_string()],
            abstention_reason: None,
            propensity: 1.0,
            explored: false,
            latency_ms: 12,
            usage: None,
            attempts: Vec::new(),
            error_class: None,
            cache_hit: false,
            response_sha256: None,
            legacy_v1: false,
        }
    }

    /// An outcome row, written directly.
    pub(crate) struct OutcomeFixture<'a> {
        pub(crate) settlement_id: &'a str,
        pub(crate) classification: &'a str,
        pub(crate) executed_action: Option<&'a str>,
        pub(crate) executed_target: Option<&'a str>,
        pub(crate) reward: Option<serde_json::Value>,
        pub(crate) cost_micro_usd: Option<i64>,
        pub(crate) provider_attempts: usize,
        pub(crate) bytes: Option<(i64, i64, i64, i64)>,
        pub(crate) denied_tool_attempts: Option<i64>,
        pub(crate) completeness: &'a str,
    }

    impl Default for OutcomeFixture<'_> {
        fn default() -> Self {
            Self {
                settlement_id: "settlement-1",
                classification: "success",
                executed_action: Some("answer"),
                executed_target: Some("none"),
                reward: None,
                cost_micro_usd: None,
                provider_attempts: 1,
                bytes: None,
                denied_tool_attempts: None,
                completeness: "complete",
            }
        }
    }

    /// Write `outcome` for `decision` into the store of `workspace_dir`.
    pub(crate) fn write_outcome(workspace_dir: &Path, decision: DecisionId, outcome: &OutcomeFixture<'_>) {
        let conn = Connection::open(db_path(workspace_dir)).expect("test: open");
        let attempts = serde_json::to_string(&vec![serde_json::json!({}); outcome.provider_attempts]).expect("test");
        let (prompt_before, prompt_after, tool_before, tool_after) = match outcome.bytes {
            Some((a, b, c, d)) => (Some(a), Some(b), Some(c), Some(d)),
            None => (None, None, None, None),
        };
        conn.execute(
            "INSERT INTO outcomes (decision_id, settlement_id, recorded_at_ms, classification, executed_action, \
             executed_target, provider_attempts_json, reward_json, cost_micro_usd, prompt_bytes_before, \
             prompt_bytes_after, tool_bytes_before, tool_bytes_after, denied_tool_attempts, completeness, \
             record_hash) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 'test')",
            params![
                decision.to_string(),
                outcome.settlement_id,
                outcome.classification,
                outcome.executed_action,
                outcome.executed_target,
                attempts,
                outcome.reward.as_ref().map(ToString::to_string),
                outcome.cost_micro_usd,
                prompt_before,
                prompt_after,
                tool_before,
                tool_after,
                outcome.denied_tool_attempts,
                outcome.completeness,
            ],
        )
        .expect("test: outcome");
    }

    /// Open a read-write store over `workspace_dir` that is not shared with
    /// other tests.
    pub(crate) fn open_private(workspace_dir: &Path, events: Arc<dyn CausalEventSink>) -> CausalDecisionStore {
        CausalDecisionStore::open(
            workspace_dir,
            Retention::days(30, 30),
            events,
            StoreMode::ReadWrite,
            WriterSetup::default(),
        )
        .expect("test: open")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::testing::{OutcomeFixture, open_private, record, write_outcome};
    use super::*;

    // Frozen pre-optimization implementation: independent owned encoding/hash oracle.
    // Keep its original field ordering and typed hash logic when changing the writer.
    struct LegacyOwnedDecision {
        columns: Vec<(&'static str, rusqlite::types::Value)>,
        record_hash: String,
    }
    impl LegacyOwnedDecision {
        fn of(record: &DecisionRecord) -> Result<Self, serde_json::Error> {
            use rusqlite::types::Value as V;
            let lineage = &record.lineage;
            let columns = vec![
                ("decision_id", V::Text(record.decision_id.to_string())),
                ("created_at_ms", V::Integer(record.created_at_ms)),
                ("workspace_id", V::Text(lineage.workspace_id.to_string())),
                (
                    "prior_decision_id",
                    text(lineage.prior_decision_id.map(|id| id.to_string()).as_deref()),
                ),
                ("owner_id", text(lineage.owner_id.as_deref())),
                ("session_key", text(lineage.session_key.as_deref())),
                ("run_id", text(lineage.run_id.as_deref())),
                ("task_id", text(lineage.task_id.as_deref())),
                ("source_event_id", text(lineage.source_event_id.as_deref())),
                ("causation_id", text(lineage.causation_id.as_deref())),
                ("channel", text(lineage.channel.as_deref())),
                ("enabled_snapshot", V::Integer(i64::from(record.enabled_snapshot))),
                ("state_fingerprint", V::Text(record.state_fingerprint.clone())),
                ("redacted_projection", text(record.redacted_projection.as_deref())),
                ("protocol_version", V::Integer(i64::from(record.protocol_version))),
                ("policy_version", V::Text(record.policy_version.clone())),
                ("catalog_generation", V::Text(record.catalog_generation.clone())),
                ("scorer_version", V::Text(record.scorer_version.clone())),
                ("backend_kind", V::Text(record.backend_kind.clone())),
                ("backend_locality", V::Text(record.backend_locality.clone())),
                ("configured_model", V::Text(record.configured_model.clone())),
                ("backend_endpoint", text(record.backend_endpoint.as_deref())),
                ("claimed_model", text(record.claimed_model.as_deref())),
                ("provider_request_id", text(record.provider_request_id.as_deref())),
                ("candidates_json", V::Text(serde_json::to_string(&record.candidates)?)),
                ("gates_json", V::Text(serde_json::to_string(&record.gates)?)),
                (
                    "distributions_json",
                    V::Text(serde_json::to_string(&record.distributions)?),
                ),
                (
                    "builtin_shadow_json",
                    V::Text(serde_json::to_string(&record.builtin_shadow)?),
                ),
                ("features_json", V::Text(serde_json::to_string(&record.features)?)),
                ("selected_candidate_id", text(record.selected_candidate_id.as_deref())),
                ("directive_kind", V::Text(record.directive_kind.clone())),
                (
                    "fallback_order_json",
                    V::Text(serde_json::to_string(&record.fallback_order)?),
                ),
                ("abstention_reason", text(record.abstention_reason.as_deref())),
                ("propensity", V::Real(record.propensity)),
                ("explored", V::Integer(i64::from(record.explored))),
                ("latency_ms", integer(record.latency_ms)),
                (
                    "usage_json",
                    match &record.usage {
                        Some(usage) => V::Text(serde_json::to_string(usage)?),
                        None => V::Null,
                    },
                ),
                ("attempts_json", V::Text(serde_json::to_string(&record.attempts)?)),
                ("error_class", text(record.error_class.as_deref())),
                ("cache_hit", V::Integer(i64::from(record.cache_hit))),
                ("response_sha256", text(record.response_sha256.as_deref())),
                ("legacy_v1", V::Integer(i64::from(record.legacy_v1))),
            ];
            // The hash covers a fixed, versioned list of columns in a fixed
            // order, so the same record always hashes the same (the idempotency
            // comparison), and a column a later schema adds never turns a resent
            // record into a false conflict (D-105 K-2).
            let mut hasher = Sha256::new();
            hasher.update(RECORD_HASH_DOMAIN);
            for (name, value) in columns.iter().filter(|(name, _)| RECORD_HASH_COLUMNS.contains(name)) {
                hasher.update(name.as_bytes());
                hasher.update([0]);
                match value {
                    V::Null => hasher.update(b"n"),
                    V::Integer(number) => {
                        hasher.update(b"i");
                        hasher.update(number.to_be_bytes());
                    }
                    V::Real(number) => {
                        hasher.update(b"r");
                        hasher.update(number.to_bits().to_be_bytes());
                    }
                    V::Text(text) => {
                        hasher.update(b"t");
                        hasher.update((text.len() as u64).to_be_bytes());
                        hasher.update(text.as_bytes());
                    }
                    V::Blob(bytes) => {
                        hasher.update(b"b");
                        hasher.update((bytes.len() as u64).to_be_bytes());
                        hasher.update(bytes);
                    }
                }
            }
            Ok(Self {
                columns,
                record_hash: hex::encode(hasher.finalize()),
            })
        }
    }

    fn encoding_fixture(rich: bool) -> DecisionRecord {
        let mut value = record(id(21), "prx_encoding_workspace", Some("prx_user"), 10);
        if rich {
            value.lineage.prior_decision_id = Some(id(20));
            value.lineage.session_key = Some(Arc::from("session"));
            value.lineage.task_id = Some(Arc::from("task"));
            value.lineage.source_event_id = Some(Arc::from("source"));
            value.lineage.causation_id = Some(Arc::from("cause"));
            value.lineage.channel = Some(Arc::from("channel"));
            value.redacted_projection = Some("quoted \"\\ \u{03bb}\n".to_owned());
            value.backend_endpoint = Some("https://example.com/v1".to_owned());
            value.claimed_model = Some(String::new());
            value.provider_request_id = Some("request".to_owned());
            value.abstention_reason = Some("reason".to_owned());
            value.error_class = Some("refused".to_owned());
            value.response_sha256 = Some("a".repeat(64));
            value.usage = Some(serde_json::json!({"input_tokens": 3, "output_tokens": 2}));
            value.gates = serde_json::json!([{ "pass": false, "reason": "fixed" }]);
            value.features = serde_json::json!({"value": [null, true, "\u{03bb}"]});
            value.propensity = 0.25;
            value.latency_ms = u64::MAX;
            value.explored = true;
            value.cache_hit = true;
            value.legacy_v1 = true;
        }
        value
    }

    #[test]
    fn borrowed_decision_encoding_matches_frozen_owned_columns_and_hash() {
        for rich in [false, true] {
            let record = encoding_fixture(rich);
            let actual = EncodedDecision::of(&record).expect("borrowed encoding");
            let legacy = LegacyOwnedDecision::of(&record).expect("legacy encoding");
            assert_eq!(actual.columns.len(), legacy.columns.len());
            for ((name, value), (old_name, old_value)) in actual.columns.iter().zip(&legacy.columns) {
                assert_eq!(name, old_name, "column ordering");
                assert_eq!(
                    value.as_value_ref(),
                    rusqlite::types::ValueRef::from(old_value),
                    "column {name}"
                );
            }
            assert_eq!(actual.record_hash, legacy.record_hash, "versioned record hash");
            let mut changed = record.clone();
            changed.policy_version.push_str("-changed");
            assert_ne!(
                EncodedDecision::of(&changed).expect("changed encoding").record_hash,
                legacy.record_hash
            );
        }
    }

    #[test]
    fn fixed_decision_insert_preserves_owned_sqlite_rows_replay_and_conflicts() {
        let mut conn = Connection::open_in_memory().expect("isolated SQLite");
        store_schema::migrate(&mut conn).expect("schema");
        let scope = DecisionScope::Owner {
            workspace_id: Arc::from("prx_encoding_workspace"),
            owner_id: Arc::from("prx_user"),
        };
        for rich in [false, true] {
            let mut record = encoding_fixture(rich);
            record.decision_id = if rich { id(22) } else { id(21) };
            let legacy = LegacyOwnedDecision::of(&record).expect("legacy encoding");
            assert_eq!(insert_decision(&conn, &record).expect("insert"), Inserted::Recorded);
            assert_eq!(
                insert_decision(&conn, &record).expect("repeat"),
                Inserted::AlreadyRecorded
            );
            conn.query_row(
                "SELECT * FROM decisions WHERE decision_id = ?1",
                [record.decision_id.to_string()],
                |row| {
                    for (name, expected) in &legacy.columns {
                        assert_eq!(
                            row.get_ref(*name)?,
                            rusqlite::types::ValueRef::from(expected),
                            "persisted {name}"
                        );
                    }
                    assert_eq!(row.get::<_, String>("record_hash")?, legacy.record_hash);
                    Ok(())
                },
            )
            .expect("all old column values preserved");
            let mut digest = Sha256::new();
            digest.update(b"prx.decision.terminal-replay-row.v1\0");
            for name in RECORD_HASH_COLUMNS
                .iter()
                .copied()
                .chain(["prior_decision_id", "backend_endpoint"])
            {
                let (_, value) = legacy
                    .columns
                    .iter()
                    .find(|(column, _)| *column == name)
                    .expect("legacy replay column");
                replay_column_hash(&mut digest, name, value.into());
            }
            let expected = hex::encode(digest.finalize());
            assert_eq!(
                record.replay_digest("prx_encoding_workspace").expect("issued replay"),
                expected
            );
            let stored = replay_record_on_connection(&conn, &scope, &record.decision_id)
                .expect("actual replay")
                .expect("owned row");
            assert_eq!(stored.digest, expected);
            let mut conflict = record.clone();
            conflict.lineage.prior_decision_id = Some(id(19));
            assert_eq!(
                insert_decision(&conn, &conflict).expect("prior conflict"),
                Inserted::Conflict
            );
            conflict = record.clone();
            conflict.policy_version.push_str("-different");
            assert_eq!(
                insert_decision(&conn, &conflict).expect("hash conflict"),
                Inserted::Conflict
            );
            assert_eq!(
                replay_record_on_connection(&conn, &scope, &record.decision_id)
                    .expect("reread")
                    .expect("preserved row")
                    .digest,
                expected
            );
        }
    }

    /// R-I1-confirm Note 1: a store another holder shares is not closed; the
    /// last holder's close takes it out of the registry, so the next shared
    /// open gets a fresh, writable store instead of the closed one.
    #[tokio::test]
    async fn the_last_holder_closes_and_unregisters_the_store() {
        let temp = tempfile::TempDir::new().unwrap();
        let open = || {
            CausalDecisionStore::open_shared(
                temp.path(),
                Retention::days(30, 30),
                Arc::new(RecordingSink::default()),
                StoreMode::ReadWrite,
            )
            .unwrap()
        };
        let first = open();
        let second = open();
        assert!(Arc::ptr_eq(&first, &second), "one shared store");
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        CausalDecisionStore::close(first, deadline).await;
        assert!(
            matches!(
                second.writer().unwrap().record_decision(record(
                    DecisionId::parse("0192f0c4-0000-7000-8000-000000000001").unwrap(),
                    "ws",
                    Some("a"),
                    10
                )),
                Enqueue::Queued
            ),
            "another holder keeps the store open"
        );
        // Keep the allocation reserved after the final strong reference is
        // dropped: a raw address alone can be reused by a fresh Arc.
        let closed = Arc::downgrade(&second);
        CausalDecisionStore::close(second, deadline).await;
        assert!(closed.upgrade().is_none(), "the last holder releases the closed store");
        let fresh = open();
        assert!(
            !std::sync::Weak::ptr_eq(&closed, &Arc::downgrade(&fresh)),
            "a closed store is never handed out again"
        );
        assert!(
            matches!(
                fresh.writer().unwrap().record_decision(record(
                    DecisionId::parse("0192f0c4-0000-7000-8000-000000000002").unwrap(),
                    "ws",
                    Some("a"),
                    11
                )),
                Enqueue::Queued
            ),
            "the fresh store writes"
        );
        CausalDecisionStore::close(fresh, deadline).await;
    }

    /// D-105 K-2: the record hash covers the fixed schema-1 column list: a
    /// columns added later (`backend_endpoint`, `prior_decision_id`) change nothing;
    /// every listed column does, and every listed column is a stored one.
    #[test]
    fn the_record_hash_covers_a_fixed_versioned_column_list() {
        let first_id = DecisionId::parse("0192f0c4-0000-7000-8000-000000000001").unwrap();
        let base = record(first_id, "ws", Some("a"), 10);
        let hash = |record: &DecisionRecord| EncodedDecision::of(record).unwrap().record_hash;
        let mut added = base.clone();
        added.backend_endpoint = Some("http://127.0.0.1:8900/v1".to_string());
        assert_eq!(hash(&base), hash(&added), "a later column is not hashed");
        added.lineage.prior_decision_id = Some(id(2));
        assert_eq!(hash(&base), hash(&added), "legacy record hashes stay stable");
        let mut claimed = base.clone();
        claimed.claimed_model = Some("another".to_string());
        assert_ne!(hash(&base), hash(&claimed), "a listed column is hashed");
        let stored: Vec<&str> = EncodedDecision::of(&base)
            .unwrap()
            .columns
            .iter()
            .map(|(name, _)| *name)
            .collect();
        for column in RECORD_HASH_COLUMNS {
            assert!(stored.contains(&column), "{column} is stored");
        }
        assert_eq!(
            stored
                .iter()
                .filter(|name| !RECORD_HASH_COLUMNS.contains(name))
                .collect::<Vec<_>>(),
            [&"prior_decision_id", &"backend_endpoint"],
            "only later schema columns are outside the list"
        );
    }

    #[test]
    fn prior_decision_link_is_typed_and_part_of_duplicate_conflict_check() {
        let mut conn = Connection::open_in_memory().expect("store");
        store_schema::migrate(&mut conn).expect("schema");
        let mut first = record(id(2), "ws", Some("prx_user"), 10);
        first.lineage.prior_decision_id = Some(id(1));
        assert_eq!(insert_decision(&conn, &first).expect("insert"), Inserted::Recorded);
        assert_eq!(
            insert_decision(&conn, &first).expect("repeat"),
            Inserted::AlreadyRecorded
        );
        let stored: Option<String> = conn
            .query_row(
                "SELECT prior_decision_id FROM decisions WHERE decision_id = ?1",
                [first.decision_id.to_string()],
                |row| row.get(0),
            )
            .expect("stored parent");
        let prior = id(1).to_string();
        assert_eq!(stored.as_deref(), Some(prior.as_str()));
        let columns = decision_columns(&conn).expect("columns");
        let select = format!("SELECT {columns} FROM decisions d WHERE d.decision_id = ?1");
        let read = |conn: &Connection| {
            conn.query_row(&select, [first.decision_id.to_string()], RawDecision::read)
                .expect("read row")
                .decode(Vec::new())
        };
        assert_eq!(read(&conn).expect("typed row").prior_decision_id, Some(id(1)));
        for parent in [None, Some(id(3))] {
            let mut changed = first.clone();
            changed.lineage.prior_decision_id = parent;
            assert_eq!(insert_decision(&conn, &changed).expect("conflict"), Inserted::Conflict);
        }
        let legacy = record(id(4), "ws", Some("prx_user"), 10);
        assert_eq!(insert_decision(&conn, &legacy).expect("legacy"), Inserted::Recorded);
        assert_eq!(
            insert_decision(&conn, &legacy).expect("repeat legacy"),
            Inserted::AlreadyRecorded
        );
        let mut linked = legacy;
        linked.lineage.prior_decision_id = Some(id(3));
        assert_eq!(
            insert_decision(&conn, &linked).expect("legacy conflict"),
            Inserted::Conflict
        );
        conn.execute(
            "UPDATE decisions SET prior_decision_id = 'invalid' WHERE decision_id = ?1",
            [first.decision_id.to_string()],
        )
        .expect("corrupt parent fixture");
        assert!(
            read(&conn).is_err(),
            "a malformed stored parent cannot become an unlinked row"
        );
    }

    #[tokio::test]
    async fn old_read_only_stores_project_absent_columns_without_migration() {
        for version in [1_i64, 2] {
            let temp = tempfile::TempDir::new().expect("workspace");
            let store = Arc::new(open_private(temp.path(), sink()));
            let writer = store.writer().expect("writer");
            let decision_id = id(u8::try_from(version).expect("version"));
            let original = record(decision_id, "ws", Some("prx_user"), now_ms());
            writer.record_decision(original.clone());
            flushed(&store).await;
            CausalDecisionStore::close(store, Instant::now() + std::time::Duration::from_secs(5)).await;

            let path = db_path(temp.path());
            let conn = Connection::open(&path).expect("downgrade fixture");
            conn.execute_batch(
                "DROP TABLE child_outcomes; \
                 DROP INDEX decisions_scope_time; \
                 CREATE INDEX decisions_scope_time ON decisions(workspace_id, owner_id, created_at_ms);",
            )
            .expect("remove v5 child settlements and restore the v1 scope index");
            conn.execute_batch("ALTER TABLE decisions DROP COLUMN prior_decision_id")
                .expect("remove v3 column");
            if version == 1 {
                conn.execute_batch("ALTER TABLE decisions DROP COLUMN backend_endpoint")
                    .expect("remove v2 column");
            }
            conn.pragma_update(None, "user_version", version).expect("old version");
            drop(conn);
            let before = std::fs::read(&path).expect("database bytes");
            let read_only = CausalDecisionStore::open_unshared_read_only(temp.path());
            let view = read_only
                .reader()
                .get(&owner(&read_only, "prx_user"), &decision_id)
                .expect("old schema read")
                .expect("owned decision");
            assert_eq!(view.prior_decision_id, None);
            assert_eq!(view.backend_endpoint, None);
            assert_eq!(
                read_only
                    .reader()
                    .get(&owner(&read_only, "other_user"), &decision_id)
                    .expect("scope"),
                None
            );
            assert_eq!(std::fs::read(&path).expect("unchanged database"), before);
            let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("version read");
            assert_eq!(store_schema::version(&conn).expect("version"), version);
            let hash_before: String = conn
                .query_row(
                    "SELECT record_hash FROM decisions WHERE decision_id = ?1",
                    [decision_id.to_string()],
                    |row| row.get(0),
                )
                .expect("old hash");
            drop(conn);
            drop(read_only);

            let events = sink();
            let migrated = Arc::new(open_private(
                temp.path(),
                Arc::clone(&events) as Arc<dyn CausalEventSink>,
            ));
            let view = migrated
                .reader()
                .get(&owner(&migrated, "prx_user"), &decision_id)
                .expect("migrated read")
                .expect("migrated decision");
            assert_eq!(view.prior_decision_id, None);
            migrated.writer().expect("writer").record_decision(original);
            flushed(&migrated).await;
            assert!(incomplete(&events).is_empty(), "legacy replay remains idempotent");
            CausalDecisionStore::close(migrated, Instant::now() + std::time::Duration::from_secs(5)).await;
            let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("migrated database");
            assert_eq!(
                store_schema::version(&conn).expect("migrated version"),
                store_schema::SCHEMA_VERSION
            );
            let hash_after: String = conn
                .query_row(
                    "SELECT record_hash FROM decisions WHERE decision_id = ?1",
                    [decision_id.to_string()],
                    |row| row.get(0),
                )
                .expect("migrated hash");
            assert_eq!(hash_after, hash_before);
        }
    }
    use crate::decision::telemetry::testing::RecordingSink;

    fn id(n: u8) -> DecisionId {
        DecisionId::legacy(&[n; 32])
    }

    fn sink() -> Arc<RecordingSink> {
        Arc::new(RecordingSink::default())
    }

    #[tokio::test]
    async fn writer_drain_reports_unacknowledged_and_disconnected_queue() {
        let (queue, receiver) = std::sync::mpsc::sync_channel(1);
        let writer = DecisionStoreWriter {
            queue,
            events: sink(),
            workspace_id: Arc::from("prx_workspace"),
        };
        assert!(
            !writer
                .drain(Instant::now() + std::time::Duration::from_millis(10))
                .await,
            "a queued marker without a writer acknowledgement is incomplete"
        );
        drop(receiver);
        assert!(
            !writer.drain(Instant::now()).await,
            "a disconnected writer is incomplete"
        );
    }

    async fn flushed(store: &CausalDecisionStore) {
        store
            .writer()
            .expect("test: writer")
            // Long enough for thousands of queued inserts on a loaded
            // machine: a test that proceeds before the drain would read a
            // half-written store.
            .drain(Instant::now() + std::time::Duration::from_mins(2))
            .await;
    }

    fn observed_outcome(decision_id: DecisionId) -> OutcomeRecord {
        OutcomeRecord {
            decision_id,
            settlement_id: "prx_terminal_1".to_owned(),
            recorded_at_ms: now_ms(),
            classification: OutcomeClassification::Refusal,
            executed_action: Some("refuse".to_owned()),
            executed_target: Some("none".to_owned()),
            provider_attempts: Some(Vec::new()),
            tool_calls: None,
            capability_ids: None,
            approvals: None,
            tokens_in: None,
            tokens_out: None,
            latency_ms: Some(3),
            cost_micro_usd: None,
            idempotency_status: None,
            reward: None,
            prompt_bytes_before: None,
            prompt_bytes_after: None,
            tool_bytes_before: None,
            tool_bytes_after: None,
            denied_tool_attempts: None,
            completeness: OutcomeCompleteness::Partial,
            coverage: None,
        }
    }

    #[tokio::test]
    async fn capability_latency_uses_only_complete_single_call_owner_and_generation_samples() {
        let temp = tempfile::tempdir().expect("workspace");
        let store = open_private(temp.path(), sink() as Arc<dyn CausalEventSink>);
        let writer = store.writer().expect("writer");
        let target_id = "native_tool:prx_project:read";
        let sparse_id = "native_tool:prx_project:sparse";
        let now = now_ms();
        for n in 1_u8..=14 {
            let decision_id = id(n);
            let owner = if n == 10 { "foreign_owner" } else { "prx_user" };
            let mut decision = record(decision_id, "ws", Some(owner), now + i64::from(n));
            if n == 11 {
                decision.catalog_generation = "old_catalog".to_owned();
            }
            assert_eq!(writer.record_decision(decision), Enqueue::Queued);
            let capability = if (6..=9).contains(&n) { sparse_id } else { target_id };
            let mut outcome = observed_outcome(decision_id);
            outcome.settlement_id = format!("prx_terminal_{n}");
            outcome.classification = OutcomeClassification::Success;
            outcome.executed_action = Some("use_tool".to_owned());
            outcome.executed_target = Some("native_tool".to_owned());
            outcome.capability_ids = Some(vec![capability.to_owned()]);
            outcome.tool_calls = Some(vec![RecordedToolCall {
                capability_id: capability.to_owned(),
                status: "success".to_owned(),
            }]);
            if n == 13 {
                outcome.tool_calls.as_mut().expect("calls").push(RecordedToolCall {
                    capability_id: capability.to_owned(),
                    status: "success".to_owned(),
                });
            }
            if n == 14 {
                outcome.classification = OutcomeClassification::Failed;
            }
            outcome.approvals = Some(Vec::new());
            outcome.coverage = Some(1.0);
            outcome.completeness = if n == 12 {
                OutcomeCompleteness::Partial
            } else {
                OutcomeCompleteness::Complete
            };
            outcome.latency_ms = Some(u64::from(n) * 10);
            assert_eq!(writer.record_outcome(outcome), Enqueue::Queued);
        }
        flushed(&store).await;
        assert_eq!(store.reader().counts().expect("committed rows").outcomes, 14);
        let workspace_id = store.workspace_id().expect("stable workspace id");
        let p50 = store
            .reader()
            .capability_latency_p50(&workspace_id, "prx_user", "catalog-1")
            .expect("scoped complete samples");
        assert_eq!(p50.get(target_id), Some(&30));
        assert!(!p50.contains_key(sparse_id), "four samples retain the family default");
        assert!(
            store
                .reader()
                .capability_latency_p50(&workspace_id, "foreign_owner", "catalog-1")
                .expect("foreign scope")
                .is_empty()
        );
        let plan = store
            .reader()
            .with(Vec::<String>::new(), |conn| {
                let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {CAPABILITY_LATENCY_SQL}"))?;
                let mut rows = statement.query(params![workspace_id.as_ref(), "prx_user", "catalog-1"])?;
                let mut details = Vec::new();
                while let Some(row) = rows.next()? {
                    details.push(row.get(3)?);
                }
                Ok(details)
            })
            .expect("bounded history query plan");
        assert!(
            plan.iter().any(|step| step.contains("decisions_scope_time")),
            "owner history must use the scope/time index: {plan:?}"
        );
        let recent_plan = store
            .reader()
            .with(Vec::<String>::new(), |conn| {
                let mut statement = conn.prepare(
                    "EXPLAIN QUERY PLAN SELECT decision_id, catalog_generation, created_at_ms
                     FROM decisions WHERE workspace_id = ?1 AND owner_id = ?2
                     ORDER BY created_at_ms DESC, decision_id DESC LIMIT 1024",
                )?;
                let mut rows = statement.query(params![workspace_id.as_ref(), "prx_user"])?;
                let mut details = Vec::new();
                while let Some(row) = rows.next()? {
                    details.push(row.get(3)?);
                }
                Ok(details)
            })
            .expect("recent-decision index order");
        assert!(
            recent_plan.iter().any(|step| step.contains("decisions_scope_time"))
                && recent_plan.iter().all(|step| !step.contains("TEMP B-TREE")),
            "the bounded recent scan must avoid sorting a timestamp tie: {recent_plan:?}"
        );
    }

    #[tokio::test]
    async fn old_read_only_store_keeps_default_latency_without_a_tie_order_index() {
        let temp = tempfile::tempdir().expect("workspace");
        let store = Arc::new(open_private(temp.path(), sink() as Arc<dyn CausalEventSink>));
        let writer = store.writer().expect("writer");
        let capability = "native_tool:prx_project:read";
        for n in 80_u8..85 {
            let decision_id = id(n);
            assert_eq!(
                writer.record_decision(record(decision_id, "ws", Some("prx_user"), now_ms() + i64::from(n))),
                Enqueue::Queued
            );
        }
        flushed(&store).await;
        for n in 80_u8..85 {
            let decision_id = id(n);
            let mut outcome = observed_outcome(decision_id);
            outcome.settlement_id = format!("prx_terminal_{n}");
            outcome.classification = OutcomeClassification::Success;
            outcome.executed_action = Some("use_tool".to_owned());
            outcome.executed_target = Some("native_tool".to_owned());
            outcome.capability_ids = Some(vec![capability.to_owned()]);
            outcome.tool_calls = Some(vec![RecordedToolCall {
                capability_id: capability.to_owned(),
                status: "success".to_owned(),
            }]);
            outcome.approvals = Some(Vec::new());
            outcome.coverage = Some(1.0);
            outcome.completeness = OutcomeCompleteness::Complete;
            outcome.latency_ms = Some(30);
            assert_eq!(writer.record_outcome(outcome), Enqueue::Queued);
        }
        flushed(&store).await;
        let workspace_id = store.workspace_id().expect("stable workspace id");
        assert_eq!(
            store
                .reader()
                .capability_latency_p50(&workspace_id, "prx_user", "catalog-1")
                .expect("current schema")
                .get(capability),
            Some(&30)
        );
        drop(writer);
        CausalDecisionStore::close(store, Instant::now() + std::time::Duration::from_secs(5)).await;
        let path = db_path(temp.path());
        let conn = Connection::open(&path).expect("version-three fixture");
        conn.execute_batch(
            "DROP INDEX decisions_scope_time;
             CREATE INDEX decisions_scope_time ON decisions(workspace_id, owner_id, created_at_ms);",
        )
        .expect("old index");
        conn.pragma_update(None, "user_version", 3).expect("old version");
        drop(conn);
        let before = std::fs::read(&path).expect("database bytes");
        let old = CausalDecisionStore::open_unshared_read_only(temp.path());
        assert!(
            old.reader()
                .capability_latency_p50(&workspace_id, "prx_user", "catalog-1")
                .expect("old read-only schema")
                .is_empty(),
            "old index must retain the versioned family latency default"
        );
        assert_eq!(std::fs::read(&path).expect("immutable database"), before);
    }

    #[tokio::test]
    async fn late_outcome_joins_once_and_a_conflicting_settlement_is_refused() {
        let temp = tempfile::tempdir().expect("workspace");
        let events = sink();
        let decision_id = id(67);
        let first = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
        let writer = first.writer().expect("writer");
        assert_eq!(
            writer.record_decision(record(decision_id, "ws", Some("prx_user"), now_ms())),
            Enqueue::Queued
        );
        flushed(&first).await;
        writer.close(Instant::now() + std::time::Duration::from_secs(5)).await;
        drop(writer);
        drop(first);

        let reopened = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
        let writer = reopened.writer().expect("reopened writer");
        let outcome = observed_outcome(decision_id);
        assert_eq!(writer.record_outcome(outcome.clone()), Enqueue::Queued);
        flushed(&reopened).await;
        let row = reopened
            .reader()
            .get(&operator(&reopened), &decision_id)
            .expect("read")
            .expect("decision");
        assert_eq!(row.outcomes.len(), 1);
        assert_eq!(row.outcomes.first().expect("outcome").classification, "refusal");
        assert_eq!(reopened.reader().counts().expect("counts").outcomes, 1);

        let mut replay = outcome.clone();
        replay.recorded_at_ms = outcome.recorded_at_ms + 43_000;
        assert_eq!(writer.record_outcome(replay), Enqueue::Queued);
        let mut conflicting = outcome.clone();
        conflicting.cost_micro_usd = Some(7);
        assert_eq!(writer.record_outcome(conflicting), Enqueue::Queued);
        assert_eq!(writer.record_outcome(observed_outcome(id(68))), Enqueue::Queued);
        flushed(&reopened).await;
        assert_eq!(reopened.reader().counts().expect("counts").outcomes, 1);
        let stored_at: i64 = Connection::open(db_path(temp.path()))
            .expect("database")
            .query_row(
                "SELECT recorded_at_ms FROM outcomes WHERE decision_id = ?1",
                [decision_id.to_string()],
                |row| row.get(0),
            )
            .expect("first outcome timestamp");
        assert_eq!(
            stored_at, outcome.recorded_at_ms,
            "a delayed replay retains the first arrival timestamp"
        );
        assert_eq!(
            events
                .events
                .lock()
                .iter()
                .filter(|event| event.kind == CausalEventKind::OutcomeRecorded)
                .count(),
            1,
            "only a new committed row emits the recorded event"
        );
        assert_eq!(
            incomplete(&events),
            [IncompleteReason::Conflict, IncompleteReason::UnknownDecision]
        );
    }

    #[tokio::test]
    async fn partial_outcome_at_provider_retention_boundary_reopens_and_replays() {
        let temp = tempfile::tempdir().expect("workspace");
        let events = sink();
        let decision_id = id(70);
        let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
        let writer = store.writer().expect("writer");
        assert_eq!(
            writer.record_decision(record(decision_id, "ws", Some("prx_user"), now_ms())),
            Enqueue::Queued
        );
        flushed(&store).await;
        let mut outcome = observed_outcome(decision_id);
        outcome.provider_attempts = Some(
            (1..=128)
                .map(|seq| RecordedOutcomeAttempt {
                    seq,
                    status: "success".to_owned(),
                    error_class: None,
                })
                .collect(),
        );
        assert_eq!(outcome.completeness, OutcomeCompleteness::Partial);
        assert_eq!(outcome.coverage, None);
        assert_eq!(writer.record_outcome(outcome.clone()), Enqueue::Queued);
        flushed(&store).await;
        writer.close(Instant::now() + std::time::Duration::from_secs(5)).await;
        drop(writer);
        drop(store);
        let reopened = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
        let writer = reopened.writer().expect("reopened writer");
        let row = reopened
            .reader()
            .get(&operator(&reopened), &decision_id)
            .expect("read")
            .expect("decision");
        assert_eq!(row.outcomes.len(), 1);
        let retained = row.outcomes.first().expect("bounded outcome");
        assert_eq!(retained.provider_attempts, 128);
        assert_eq!(retained.completeness, "partial");
        assert!(
            reopened
                .reader()
                .outcome_matches(&operator(&reopened), &outcome)
                .expect("exact stored payload")
        );
        assert!(incomplete(&events).is_empty(), "expected an empty value");
        assert_eq!(writer.record_outcome(outcome.clone()), Enqueue::Queued);
        flushed(&reopened).await;
        assert_eq!(reopened.reader().counts().expect("counts").outcomes, 1);
        assert!(
            reopened
                .reader()
                .outcome_matches(&operator(&reopened), &outcome)
                .expect("exact replay payload")
        );
        assert!(incomplete(&events).is_empty(), "expected an empty value");
        outcome
            .provider_attempts
            .as_mut()
            .expect("attempts")
            .push(RecordedOutcomeAttempt {
                seq: 129,
                status: "success".to_owned(),
                error_class: None,
            });
        assert_eq!(writer.record_outcome(outcome), Enqueue::Dropped);
        flushed(&reopened).await;
        assert_eq!(reopened.reader().counts().expect("counts").outcomes, 1);
        assert_eq!(incomplete(&events), [IncompleteReason::Conflict]);
    }

    #[tokio::test]
    async fn outcome_queue_refuses_oversized_or_raw_metadata_without_a_row() {
        assert!(bounded_capability_id("mcp:prx_space:search/r\u{e9}sum\u{e9}"));
        assert!(!bounded_capability_id("mcp:prx_space:private:extra"));
        let temp = tempfile::tempdir().expect("workspace");
        let events = sink();
        let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
        let writer = store.writer().expect("writer");
        let decision_id = id(69);
        assert_eq!(
            writer.record_decision(record(decision_id, "ws", Some("prx_user"), now_ms())),
            Enqueue::Queued
        );
        flushed(&store).await;
        let base = observed_outcome(decision_id);
        let private_marker = "private@example.com";
        let mut invalid = Vec::new();
        let mut raw = base.clone();
        raw.settlement_id = format!("prx_{private_marker}");
        invalid.push(raw);
        let mut oversized = base.clone();
        oversized.settlement_id = "x".repeat(200_000);
        invalid.push(oversized);
        let mut action = base.clone();
        action.executed_action = Some("raw user text".to_owned());
        invalid.push(action);
        let mut attempts = base.clone();
        attempts.provider_attempts = Some(
            (1..=129)
                .map(|seq| RecordedOutcomeAttempt {
                    seq,
                    status: "failed".to_owned(),
                    error_class: Some("unavailable".to_owned()),
                })
                .collect(),
        );
        invalid.push(attempts);
        let mut negative_cost = base.clone();
        negative_cost.cost_micro_usd = Some(-1);
        invalid.push(negative_cost);
        let mut negative_time = base.clone();
        negative_time.recorded_at_ms = -1;
        invalid.push(negative_time);
        for outcome in &invalid {
            assert!(!format!("{outcome:?}").contains(private_marker));
        }
        for outcome in invalid {
            assert_eq!(writer.record_outcome(outcome), Enqueue::Dropped);
        }
        flushed(&store).await;
        assert_eq!(store.reader().counts().expect("counts").outcomes, 0);
        assert_eq!(incomplete(&events), vec![IncompleteReason::Conflict; 6]);
    }

    /// The local operator's scope of `store`.
    fn operator(store: &CausalDecisionStore) -> DecisionScope {
        DecisionScope::LocalOperator {
            workspace_id: store.workspace_id().unwrap_or_else(|| Arc::from("no store")),
        }
    }

    /// `owner`'s scope in `store`.
    fn owner(store: &CausalDecisionStore, owner: &str) -> DecisionScope {
        DecisionScope::Owner {
            workspace_id: store.workspace_id().expect("test: id"),
            owner_id: Arc::from(owner),
        }
    }

    fn incomplete(sink: &RecordingSink) -> Vec<IncompleteReason> {
        sink.events
            .lock()
            .iter()
            .filter_map(|event| event.fields.incomplete)
            .collect()
    }

    /// V8.04: the same record twice is stored once; a different record
    /// under the same id is refused and reported, the first kept.
    #[tokio::test]
    async fn a_conflicting_decision_is_refused_not_overwritten() {
        let temp = tempfile::TempDir::new().unwrap();
        let events = sink();
        let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
        let writer = store.writer().unwrap();
        let first = record(id(1), "ws", Some("alice"), now_ms());
        assert_eq!(writer.record_decision(first.clone()), Enqueue::Queued);
        assert_eq!(writer.record_decision(first.clone()), Enqueue::Queued);
        let mut changed = first.clone();
        changed.directive_kind = "refuse".to_string();
        writer.record_decision(changed);
        flushed(&store).await;
        let view = store.reader().get(&operator(&store), &id(1)).unwrap().unwrap();
        assert_eq!(view.directive_kind, "answer", "the stored record is kept");
        assert_eq!(store.reader().counts().unwrap().decisions, 1);
        assert_eq!(incomplete(&events), [IncompleteReason::Conflict]);
    }

    #[tokio::test]
    async fn decision_debug_omits_private_endpoint_projection_and_lineage() {
        let temp = tempfile::TempDir::new().expect("workspace");
        let events = sink();
        let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
        let mut entry = record(id(55), "prx_private_workspace", Some("prx_user"), now_ms());
        entry.backend_endpoint = Some("http://127.0.0.1/prx_private_route".to_string());
        entry.redacted_projection = Some("{\"goal\":\"prx_private_goal\"}".to_string());
        let record_debug = format!("{entry:?}");
        for private in ["prx_private_workspace", "prx_private_route", "prx_private_goal"] {
            assert!(!record_debug.contains(private));
        }
        assert_eq!(store.writer().expect("writer").record_decision(entry), Enqueue::Queued);
        flushed(&store).await;
        let view = store
            .reader()
            .get(&operator(&store), &id(55))
            .expect("read")
            .expect("stored");
        let view_debug = format!("{view:?}");
        for private in ["prx_private_workspace", "prx_private_route", "prx_private_goal"] {
            assert!(!view_debug.contains(private));
        }
        assert_eq!(
            view.backend_endpoint.as_deref(),
            Some("http://127.0.0.1/prx_private_route")
        );
    }

    #[tokio::test]
    async fn decision_chain_transaction_rolls_back_on_conflict_and_emits_commit_after_storage() {
        let temp = tempfile::TempDir::new().expect("workspace");
        let events = sink();
        let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
        let writer = store.writer().expect("writer");
        let existing = record(id(2), "ws", Some("prx_user"), 10);
        writer.record_decision(existing.clone());
        flushed(&store).await;

        let first = record(id(1), "ws", Some("prx_user"), 10);
        let mut conflicting_second = existing.clone();
        conflicting_second.lineage.prior_decision_id = Some(first.decision_id);
        assert_eq!(
            writer.record_chain(vec![first.clone(), conflicting_second]),
            Enqueue::Queued
        );
        flushed(&store).await;
        assert!(
            store
                .reader()
                .get(&operator(&store), &first.decision_id)
                .unwrap()
                .is_none()
        );
        assert_eq!(store.reader().counts().unwrap().decisions, 1);
        assert_eq!(
            incomplete(&events),
            [IncompleteReason::Conflict, IncompleteReason::Conflict]
        );

        let mut second = record(id(3), "ws", Some("prx_user"), 10);
        second.lineage.prior_decision_id = Some(first.decision_id);
        let committed_before = events
            .events
            .lock()
            .iter()
            .filter(|event| event.kind == CausalEventKind::DecisionCommitted)
            .count();
        assert_eq!(
            writer.record_chain(vec![first.clone(), second.clone()]),
            Enqueue::Queued
        );
        flushed(&store).await;
        assert_eq!(store.reader().counts().unwrap().decisions, 3);
        assert_eq!(
            store
                .reader()
                .get(&operator(&store), &second.decision_id)
                .unwrap()
                .unwrap()
                .prior_decision_id,
            Some(first.decision_id)
        );
        let committed_after = events
            .events
            .lock()
            .iter()
            .filter(|event| event.kind == CausalEventKind::DecisionCommitted)
            .count();
        assert_eq!(committed_after - committed_before, 2);
        writer.record_chain(vec![first, second]);
        flushed(&store).await;
        assert_eq!(
            events
                .events
                .lock()
                .iter()
                .filter(|event| event.kind == CausalEventKind::DecisionCommitted)
                .count(),
            committed_after,
            "idempotent replay does not emit another commit"
        );
    }

    #[tokio::test]
    async fn decision_chain_rejects_orphan_and_foreign_scope_before_queueing() {
        let temp = tempfile::TempDir::new().expect("workspace");
        let events = sink();
        let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
        let writer = store.writer().expect("writer");
        let first = record(id(10), "ws", Some("prx_user"), 10);
        let mut second = record(id(11), "ws", Some("prx_user"), 10);
        second.lineage.prior_decision_id = Some(first.decision_id);

        let mut orphan = record(id(12), "ws", Some("prx_user"), 10);
        orphan.lineage.prior_decision_id = Some(id(99));
        assert_eq!(writer.record_decision(orphan), Enqueue::Dropped);

        let mut invalid = second.clone();
        invalid.lineage.owner_id = Some(Arc::from("other_user"));
        assert_eq!(writer.record_chain(vec![first.clone(), invalid]), Enqueue::Dropped);
        let mut invalid = second.clone();
        invalid.lineage.run_id = Some(Arc::from("other_run"));
        assert_eq!(writer.record_chain(vec![first.clone(), invalid]), Enqueue::Dropped);
        let mut invalid = second.clone();
        invalid.lineage.source_event_id = Some(Arc::from("other_event"));
        assert_eq!(writer.record_chain(vec![first.clone(), invalid]), Enqueue::Dropped);
        let mut invalid = second.clone();
        invalid.lineage.prior_decision_id = Some(id(99));
        assert_eq!(writer.record_chain(vec![first.clone(), invalid]), Enqueue::Dropped);
        let mut invalid = second;
        invalid.decision_id = first.decision_id;
        assert_eq!(writer.record_chain(vec![first.clone(), invalid]), Enqueue::Dropped);

        flushed(&store).await;
        assert_eq!(store.reader().counts().unwrap().decisions, 0);
        assert!(
            events
                .events
                .lock()
                .iter()
                .all(|event| event.kind != CausalEventKind::DecisionCommitted)
        );
        assert_eq!(writer.record_decision(first), Enqueue::Queued);
        flushed(&store).await;
        assert_eq!(store.reader().counts().unwrap().decisions, 1);
    }

    /// V6.04, store half: another owner's decision reads exactly like an
    /// unknown one; the local operator sees every decision of the
    /// workspace, including those without an owner; another workspace sees
    /// nothing.
    #[tokio::test]
    async fn inspect_other_owner_decision_denied() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = open_private(temp.path(), sink());
        let writer = store.writer().unwrap();
        writer.record_decision(record(id(1), "ws", Some("alice"), now_ms()));
        writer.record_decision(record(id(2), "ws", None, now_ms()));
        flushed(&store).await;
        let reader = store.reader();
        let alice = owner(&store, "alice");
        let bob = owner(&store, "bob");
        assert!(reader.get(&alice, &id(1)).unwrap().is_some());
        let foreign = reader.get(&bob, &id(1));
        let unknown = reader.get(&bob, &id(9));
        assert!(matches!(foreign, Ok(None)) && matches!(unknown, Ok(None)));
        assert!(reader.get(&alice, &id(2)).unwrap().is_none(), "no owner: operator only");
        assert!(reader.get(&operator(&store), &id(2)).unwrap().is_some());
        assert!(
            reader
                .get(
                    &DecisionScope::LocalOperator {
                        workspace_id: Arc::from("another-workspace"),
                    },
                    &id(1),
                )
                .unwrap()
                .is_none()
        );
    }

    /// V6.10: the files are private; a corrupt database is moved aside
    /// (0600), a fresh store works, and the recovery is reported.
    #[tokio::test]
    async fn store_permissions_and_corrupt_db_recovery() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let path = db_path(temp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            b"this is not a sqlite database, only text that looks nothing like one",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let events = sink();
        let store = open_private(temp.path(), Arc::clone(&events) as Arc<dyn CausalEventSink>);
        assert_eq!(incomplete(&events), [IncompleteReason::StoreRecovered]);
        let aside: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("decisions.db.corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1);
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&aside[0].path()), 0o600);
        store
            .writer()
            .unwrap()
            .record_decision(record(id(1), "ws", Some("a"), now_ms()));
        flushed(&store).await;
        assert_eq!(store.reader().counts().unwrap().decisions, 1);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        for suffix in ["-wal", "-shm"] {
            let side = side_file(&path, suffix);
            if side.exists() {
                assert_eq!(mode(&side), 0o600, "{suffix}");
            }
        }
    }

    /// A database written by a newer build opens read-only: no writer, reads
    /// work, and the file is not changed.
    #[tokio::test]
    async fn a_newer_schema_opens_read_only() {
        let temp = tempfile::TempDir::new().unwrap();
        {
            let store = open_private(temp.path(), sink());
            store
                .writer()
                .unwrap()
                .record_decision(record(id(1), "ws", Some("a"), now_ms()));
            flushed(&store).await;
        }
        let path = db_path(temp.path());
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", store_schema::SCHEMA_VERSION + 1)
                .unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let store = open_private(temp.path(), sink());
        assert_eq!(store.mode(), StoreMode::ReadOnly);
        assert!(store.writer().is_none());
        assert!(store.reader().get(&operator(&store), &id(1)).unwrap().is_some());
        drop(store);
        assert_eq!(std::fs::read(&path).unwrap(), before, "the newer database is untouched");
    }

    /// A read-only store never creates a database: a missing one reads as
    /// empty.
    #[test]
    fn a_read_only_store_creates_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = CausalDecisionStore::open(
            temp.path(),
            Retention::days(30, 30),
            sink(),
            StoreMode::ReadOnly,
            WriterSetup::default(),
        )
        .unwrap();
        assert!(store.writer().is_none());
        assert!(store.reader().get(&operator(&store), &id(1)).unwrap().is_none());
        assert!(store.workspace_id().is_none(), "no store, no id");
        assert_eq!(store.reader().counts().unwrap(), StoreCounts::default());
        assert!(!temp.path().join("self").exists(), "nothing was created");
    }

    /// Retention deletes everything past the horizon in bounded batches,
    /// and the outcomes of deleted decisions go with them.
    #[tokio::test]
    async fn retention_cleanup_bounded() {
        let temp = tempfile::TempDir::new().unwrap();
        let old = now_ms() - 40 * DAY_MS;
        {
            let store = open_private(temp.path(), sink());
            let writer = store.writer().unwrap();
            for n in 0..2_500_u32 {
                let mut digest = [0_u8; 32];
                digest[..4].copy_from_slice(&n.to_be_bytes());
                let queued = loop {
                    if writer.record_decision(record(DecisionId::legacy(&digest), "ws", Some("a"), old))
                        == Enqueue::Queued
                    {
                        break true;
                    }
                    flushed(&store).await;
                };
                assert!(queued);
            }
            writer.record_decision(record(id(200), "ws", Some("a"), now_ms()));
            flushed(&store).await;
        }
        write_outcome(temp.path(), id(200), &OutcomeFixture::default());
        let mut conn = Connection::open(db_path(temp.path())).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let first_old: String = conn
            .query_row(
                "SELECT decision_id FROM decisions WHERE created_at_ms = ?1 LIMIT 1",
                [old],
                |row| row.get(0),
            )
            .unwrap();
        write_outcome(
            temp.path(),
            DecisionId::parse(&first_old).unwrap(),
            &OutcomeFixture::default(),
        );
        let deleted = prune(&mut conn, Retention::days(30, 90).cutoff(now_ms()).unwrap()).unwrap();
        assert_eq!(deleted, 2_500);
        let left: i64 = conn
            .query_row("SELECT count(*) FROM decisions", [], |row| row.get(0))
            .unwrap();
        let outcomes: i64 = conn
            .query_row("SELECT count(*) FROM outcomes", [], |row| row.get(0))
            .unwrap();
        assert_eq!((left, outcomes), (1, 1), "old outcomes cascade");
        drop(conn);

        // The store's own connection cascades too: an old decision with an
        // outcome is gone, with its outcome, after the next open.
        {
            let store = open_private(temp.path(), sink());
            store
                .writer()
                .unwrap()
                .record_decision(record(id(201), "ws", Some("a"), old));
            flushed(&store).await;
        }
        write_outcome(temp.path(), id(201), &OutcomeFixture::default());
        let store = open_private(temp.path(), sink());
        let counts = store.reader().counts().unwrap();
        assert_eq!(
            (counts.decisions, counts.outcomes),
            (1, 1),
            "pruned at open, outcome cascaded"
        );
        assert_eq!(
            Retention::days(0, 90),
            Retention::days(90, 7),
            "0 follows the fitness retention"
        );
    }

    /// D-040 OQ-A8: a full queue drops the record without blocking, counts
    /// it and reports it.
    #[tokio::test]
    async fn a_full_queue_drops_without_blocking() {
        let temp = tempfile::TempDir::new().unwrap();
        let events = sink();
        let hold = Arc::new(parking_lot::Mutex::new(()));
        let store = CausalDecisionStore::open(
            temp.path(),
            Retention::days(30, 30),
            Arc::clone(&events) as Arc<dyn CausalEventSink>,
            StoreMode::ReadWrite,
            WriterSetup {
                capacity: 1,
                hold: Some(Arc::clone(&hold)),
            },
        )
        .unwrap();
        let writer = store.writer().unwrap();
        let held = hold.lock();
        let started = Instant::now();
        let mut outcomes = Vec::new();
        for n in 1..=4 {
            outcomes.push(writer.record_decision(record(id(n), "ws", Some("a"), now_ms())));
        }
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "enqueueing never waits"
        );
        drop(held);
        assert!(outcomes.contains(&Enqueue::Dropped));
        let dropped = outcomes.iter().filter(|outcome| **outcome == Enqueue::Dropped).count();
        assert_eq!(
            incomplete(&events)
                .iter()
                .filter(|reason| **reason == IncompleteReason::StoreDropped)
                .count(),
            dropped
        );
        flushed(&store).await;
        let stored = usize::try_from(store.reader().counts().unwrap().decisions).unwrap();
        assert_eq!(stored + dropped, 4);
    }

    #[test]
    fn full_queue_drops_the_whole_decision_chain() {
        let events = sink();
        // No consumer runs until the assertions finish, so capacity one is
        // provably full without depending on the writer thread's schedule.
        let (queue, receiver) = std::sync::mpsc::sync_channel(1);
        let writer = DecisionStoreWriter {
            queue,
            events: Arc::clone(&events) as Arc<dyn CausalEventSink>,
            workspace_id: Arc::from("prx_workspace"),
        };
        assert_eq!(
            writer.record_decision(record(id(1), "ws", Some("prx_user"), 10)),
            Enqueue::Queued
        );
        let first = record(id(3), "ws", Some("prx_user"), 10);
        let mut second = record(id(4), "ws", Some("prx_user"), 10);
        second.lineage.prior_decision_id = Some(first.decision_id);
        assert_eq!(writer.record_chain(vec![first, second]), Enqueue::Dropped);
        let queued = receiver.try_recv().expect("only the filler is queued");
        assert!(
            matches!(queued, WriterMessage::DecisionBatch(records) if records.len() == 1 && records[0].decision_id == id(1))
        );
        assert!(matches!(receiver.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        assert_eq!(
            incomplete(&events)
                .iter()
                .filter(|reason| **reason == IncompleteReason::StoreDropped)
                .count(),
            2,
        );
    }

    /// The store shares one instance per database; a read-write request
    /// replaces a read-only one.
    #[test]
    fn stores_are_shared_per_database() {
        let temp = tempfile::TempDir::new().unwrap();
        let read_only =
            CausalDecisionStore::open_shared(temp.path(), Retention::days(1, 1), sink(), StoreMode::ReadOnly).unwrap();
        let again =
            CausalDecisionStore::open_shared(temp.path(), Retention::days(1, 1), sink(), StoreMode::ReadOnly).unwrap();
        assert!(Arc::ptr_eq(&read_only, &again));
        let writing =
            CausalDecisionStore::open_shared(temp.path(), Retention::days(1, 1), sink(), StoreMode::ReadWrite).unwrap();
        assert_eq!(writing.mode(), StoreMode::ReadWrite);
        let shared =
            CausalDecisionStore::open_shared(temp.path(), Retention::days(1, 1), sink(), StoreMode::ReadOnly).unwrap();
        assert!(
            Arc::ptr_eq(&writing, &shared),
            "the writer serves read-only requests too"
        );
    }

    fn listing(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .map(|entry| {
                (
                    entry.file_name().to_string_lossy().to_string(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect();
        files.sort();
        files
    }

    /// D-091 F-1: a symlinked store directory is refused before anything is
    /// created or changed through it.
    #[test]
    fn a_symlinked_store_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("self")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, workspace.join("self/causal_tree")).unwrap();
        for mode in [StoreMode::ReadWrite, StoreMode::ReadOnly] {
            let opened = CausalDecisionStore::open(
                &workspace,
                Retention::days(30, 30),
                sink(),
                mode,
                WriterSetup::default(),
            );
            assert!(matches!(opened, Err(StoreError::Unsafe(_))), "{mode:?}");
        }
        assert!(
            std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
            "nothing created through the link"
        );
        let mode = std::fs::metadata(&elsewhere).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "the target is not chmodded");
    }

    /// D-091 F-1: a symlinked database or side file is refused; the link's
    /// target keeps its bytes and mode.
    #[test]
    fn a_symlinked_database_or_side_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        for name in ["decisions.db", "decisions.db-wal", "decisions.db-shm"] {
            let temp = tempfile::TempDir::new().unwrap();
            let target = temp.path().join("target-file");
            std::fs::write(&target, b"not a database, a file that must stay as it is").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
            let dir = crate::decision::key::prepare_decision_dir(&temp.path().join("ws")).unwrap();
            std::os::unix::fs::symlink(&target, dir.join(name)).unwrap();
            for mode in [StoreMode::ReadWrite, StoreMode::ReadOnly] {
                let opened = CausalDecisionStore::open(
                    &temp.path().join("ws"),
                    Retention::days(30, 30),
                    sink(),
                    mode,
                    WriterSetup::default(),
                );
                assert!(matches!(opened, Err(StoreError::Unsafe(_))), "{name} {mode:?}");
            }
            assert_eq!(
                std::fs::read(&target).unwrap(),
                b"not a database, a file that must stay as it is"
            );
            assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
        }
    }

    /// D-091 F-5: the disabled plane's reads of an existing database leave
    /// every file of the directory as it was, and create no -wal or -shm.
    #[tokio::test]
    async fn a_read_only_open_of_an_existing_store_changes_no_file() {
        let temp = tempfile::TempDir::new().unwrap();
        {
            let store = open_private(temp.path(), sink());
            store
                .writer()
                .unwrap()
                .record_decision(record(id(1), "ws", Some("a"), now_ms()));
            flushed(&store).await;
        }
        // The writer thread closes its connection (a checkpoint) on its own
        // time; wait until the side files are gone.
        let dir = temp.path().join("self/causal_tree");
        for _ in 0..200 {
            if !dir.join("decisions.db-wal").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let before = listing(&dir);
        let store = CausalDecisionStore::open(
            temp.path(),
            Retention::days(30, 30),
            sink(),
            StoreMode::ReadOnly,
            WriterSetup::default(),
        )
        .unwrap();
        assert!(store.reader().get(&operator(&store), &id(1)).unwrap().is_some());
        assert_eq!(store.reader().counts().unwrap().decisions, 1);
        drop(store);
        assert_eq!(listing(&dir), before, "no file changed or appeared");
    }

    /// D-091 F-17: a newer database, with frames still in its WAL, is never
    /// opened for writing: neither the database nor the WAL changes.
    #[tokio::test]
    async fn a_newer_schema_with_a_live_wal_is_untouched() {
        let source = tempfile::TempDir::new().unwrap();
        {
            let store = open_private(source.path(), sink());
            flushed(&store).await;
        }
        let path = db_path(source.path());
        // Copy a newer database whose WAL still holds frames, with no
        // connection left open on the copy: a read-write open would
        // checkpoint it on close.
        let copy = tempfile::TempDir::new().unwrap();
        let copied = crate::decision::key::prepare_decision_dir(copy.path())
            .unwrap()
            .join(DB_FILE);
        {
            let holder = Connection::open(&path).unwrap();
            holder.execute_batch("PRAGMA wal_autocheckpoint = 0;").unwrap();
            holder
                .pragma_update(None, "user_version", store_schema::SCHEMA_VERSION + 1)
                .unwrap();
            for suffix in ["", "-wal"] {
                std::fs::copy(side_file(&path, suffix), side_file(&copied, suffix)).unwrap();
            }
        }
        let wal = side_file(&copied, "-wal");
        assert!(std::fs::metadata(&wal).unwrap().len() > 0, "the WAL holds frames");
        let before = (std::fs::read(&copied).unwrap(), std::fs::read(&wal).unwrap());
        let store = open_private(copy.path(), sink());
        assert_eq!(store.mode(), StoreMode::ReadOnly);
        drop(store);
        assert_eq!((std::fs::read(&copied).unwrap(), std::fs::read(&wal).unwrap()), before);
    }

    /// D-091 F-7: a horizon of 0 days keeps everything.
    #[test]
    fn a_zero_retention_keeps_everything() {
        assert_eq!(Retention::days(0, 0).cutoff(now_ms()), None);
        assert!(Retention::days(1, 0).cutoff(now_ms()).is_some());
    }

    /// D-091 F-12: a quarantine name that is taken is refused, never
    /// overwritten, and the database stays where it was.
    #[test]
    fn a_taken_quarantine_name_is_never_overwritten() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = crate::decision::key::prepare_decision_dir(temp.path()).unwrap();
        let path = dir.join(DB_FILE);
        std::fs::write(&path, b"first garbage").unwrap();
        quarantine_as(&path, "fixed").unwrap();
        std::fs::write(&path, b"second garbage").unwrap();
        assert!(quarantine_as(&path, "fixed").is_err());
        assert_eq!(
            std::fs::read(dir.join("decisions.db.corrupt-fixed")).unwrap(),
            b"first garbage"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"second garbage");
    }

    /// D-091 F-12: two recoveries get two names.
    #[test]
    fn quarantined_files_are_never_overwritten() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = crate::decision::key::prepare_decision_dir(temp.path()).unwrap();
        let path = dir.join(DB_FILE);
        for garbage in [b"first garbage".as_slice(), b"second garbage".as_slice()] {
            std::fs::write(&path, garbage).unwrap();
            quarantine(&path).unwrap();
        }
        let mut aside: Vec<Vec<u8>> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("decisions.db.corrupt-"))
            .map(|entry| std::fs::read(entry.path()).unwrap())
            .collect();
        aside.sort();
        assert_eq!(aside, [b"first garbage".to_vec(), b"second garbage".to_vec()]);
    }

    /// D-091 F-13: the scope key is the store's own id: it survives a move
    /// of the workspace, and every record carries it whatever the caller
    /// set.
    #[tokio::test]
    async fn the_workspace_id_is_stable_across_a_move() {
        let temp = tempfile::TempDir::new().unwrap();
        let first = temp.path().join("first");
        let id_before = {
            let store = open_private(&first, sink());
            store
                .writer()
                .unwrap()
                .record_decision(record(id(1), "a-caller-supplied-id", Some("a"), now_ms()));
            flushed(&store).await;
            store.workspace_id().unwrap()
        };
        let moved = temp.path().join("moved");
        std::fs::rename(&first, &moved).unwrap();
        let store = open_private(&moved, sink());
        assert_eq!(store.workspace_id().unwrap(), id_before);
        assert!(store.reader().get(&operator(&store), &id(1)).unwrap().is_some());
    }

    /// D-091 F-11: a file's progress is saved only when every decision it
    /// lists is stored.
    #[tokio::test]
    async fn import_progress_needs_every_decision_stored() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = open_private(temp.path(), sink());
        let writer = store.writer().unwrap();
        writer.record_decision(record(id(1), "ws", None, now_ms()));
        let progress = |file: &str| ImportProgress {
            file: file.to_string(),
            digest: "d".to_string(),
            lines: 1,
        };
        writer.mark_imported(progress("stored.jsonl"), vec![id(1)]);
        writer.mark_imported(progress("missing.jsonl"), vec![id(1), id(2)]);
        flushed(&store).await;
        assert!(store.reader().import_progress("stored.jsonl").unwrap().is_some());
        assert!(store.reader().import_progress("missing.jsonl").unwrap().is_none());
    }

    /// D-093 N-2: with a live WAL (a writer that has not checkpointed), the
    /// disabled plane's read sees the latest rows and creates no file.
    #[tokio::test]
    async fn a_read_only_open_sees_rows_in_a_live_wal() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = open_private(temp.path(), sink());
        store
            .writer()
            .unwrap()
            .record_decision(record(id(1), "ws", Some("a"), now_ms()));
        flushed(&store).await;
        let dir = temp.path().join("self/causal_tree");
        assert!(
            std::fs::metadata(dir.join("decisions.db-wal")).unwrap().len() > 0,
            "a live WAL"
        );
        let before: Vec<String> = listing(&dir).into_iter().map(|(name, _)| name).collect();
        let reader = CausalDecisionStore::open(
            temp.path(),
            Retention::days(30, 30),
            sink(),
            StoreMode::ReadOnly,
            WriterSetup::default(),
        )
        .unwrap();
        assert!(reader.reader().get(&operator(&reader), &id(1)).unwrap().is_some());
        let after: Vec<String> = listing(&dir).into_iter().map(|(name, _)| name).collect();
        assert_eq!(after, before, "no file appeared");
        drop(store);
    }

    /// D-093 N-6: the disabled plane's read never narrows a directory mode.
    #[tokio::test]
    async fn a_read_only_open_changes_no_directory_mode() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().unwrap();
        {
            let store = open_private(temp.path(), sink());
            flushed(&store).await;
        }
        let dir = temp.path().join("self/causal_tree");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
        let store = CausalDecisionStore::open(
            temp.path(),
            Retention::days(30, 30),
            sink(),
            StoreMode::ReadOnly,
            WriterSetup::default(),
        )
        .unwrap();
        assert_eq!(store.reader().counts().unwrap().decisions, 0);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o750);
    }

    /// R-I1-phase2 R-2: concurrent first opens of one fresh database all
    /// succeed within the bounded wait. Switching to WAL upgrades the
    /// pragma's read to a write, and SQLite skips the busy handler for that
    /// upgrade, so without a retry one of them fails at once with "database
    /// is locked".
    #[test]
    fn concurrent_first_opens_wait_for_the_wal_switch() {
        const OPENERS: usize = 8;
        for round in 0..60 {
            let temp = tempfile::TempDir::new().unwrap();
            // The writer never follows a symlink (`/var` is one on macOS).
            let path = temp.path().canonicalize().unwrap().join(DB_FILE);
            let barrier = Arc::new(std::sync::Barrier::new(OPENERS));
            let started = Instant::now();
            let handles: Vec<_> = (0..OPENERS)
                .map(|_| {
                    let (path, barrier) = (path.clone(), Arc::clone(&barrier));
                    std::thread::spawn(move || {
                        barrier.wait();
                        open_writer(&path)
                            .map(|(_, state)| state)
                            .map_err(|error| error.to_string())
                    })
                })
                .collect();
            for handle in handles {
                assert_eq!(handle.join().unwrap(), Ok(SchemaState::Current), "round {round}");
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "round {round}: the wait is bounded"
            );
        }
    }
}

fn replay_record_on_connection(
    conn: &Connection,
    scope: &DecisionScope,
    id: &DecisionId,
) -> Result<Option<DecisionReplayRecord>, StoreError> {
    replay_record_on_connection_with_deadline(conn, scope, id, None)
}

fn replay_record_on_connection_with_deadline(
    conn: &Connection,
    scope: &DecisionScope,
    id: &DecisionId,
    deadline: Option<Instant>,
) -> Result<Option<DecisionReplayRecord>, StoreError> {
    if store_schema::version(conn)? < 3 {
        return Ok(None);
    }
    let names: Vec<_> = RECORD_HASH_COLUMNS
        .iter()
        .copied()
        .chain(["prior_decision_id", "backend_endpoint"])
        .collect();
    let replay_columns = names
        .iter()
        .map(|name| format!("d.{name} AS replay_{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    let (clause, [first, second]) = scope.clause();
    let columns = decision_columns(conn)?;
    let sql = format!("SELECT {columns}, {replay_columns} FROM decisions d WHERE d.decision_id = ?1 AND {clause}");
    let raw = conn
        .prepare_cached(&sql)?
        .query_row(params![id.to_string(), first, second], |row| {
            let mut digest = Sha256::new();
            digest.update(b"prx.decision.terminal-replay-row.v1\0");
            for name in &names {
                let column = format!("replay_{name}");
                replay_column_hash(&mut digest, name, row.get_ref(column.as_str())?);
            }
            // Collection evidence remains inside the existing digest-covered gates
            // column. Old rows take no new JSON decode or canonical encoding path.
            if row.get_ref("replay_directive_kind")?.as_str()? == "collect_approval_arguments" {
                let raw = row.get_ref("replay_gates_json")?.as_str()?;
                if raw.len() > 1_048_576 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                let gates: serde_json::Value = match deadline {
                    Some(deadline) => recovery_json(raw, deadline).map_err(|_| rusqlite::Error::InvalidQuery)?,
                    None => serde_json::from_str(raw).map_err(|_| rusqlite::Error::InvalidQuery)?,
                };
                let state: String = row.get("replay_state_fingerprint")?;
                let generation: String = row.get("replay_catalog_generation")?;
                if !super::approval_collection::valid_projection(&gates, &state, &generation) {
                    return Err(rusqlite::Error::InvalidQuery);
                }
            }
            Ok((
                RawDecision::read(row)?,
                row.get::<_, Option<String>>("replay_source_event_id")?,
                row.get::<_, String>("replay_catalog_generation")?,
                row.get::<_, String>("replay_state_fingerprint")?,
                hex::encode(digest.finalize()),
            ))
        })
        .optional()?;
    raw.map(
        |(row, source_event_id, catalog_generation, state_fingerprint, digest)| {
            Ok(DecisionReplayRecord {
                decision: match deadline {
                    Some(deadline) => row.decode_until(Vec::new(), deadline)?,
                    None => row.decode(Vec::new())?,
                },
                source_event_id,
                catalog_generation,
                state_fingerprint,
                digest,
            })
        },
    )
    .transpose()
}

fn outcome_matches_on_connection(
    conn: &Connection,
    scope: &DecisionScope,
    record: &OutcomeRecord,
) -> Result<bool, StoreError> {
    let encoded = EncodedOutcome::of(record)?;

    let (clause, [first, second]) = scope.clause();
    let columns = encoded
        .columns
        .iter()
        .filter(|(name, _)| *name != "recorded_at_ms")
        .map(|(name, _)| format!("o.{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {columns} FROM outcomes o JOIN decisions d ON d.decision_id = o.decision_id \
                 WHERE d.decision_id = ?1 AND {clause} AND o.settlement_id = ?4"
    );
    Ok(conn
        .prepare_cached(&sql)?
        .query_row(
            params![record.decision_id.to_string(), first, second, record.settlement_id],
            |row| {
                for (name, expected) in encoded.columns.iter().filter(|(name, _)| *name != "recorded_at_ms") {
                    let stored: rusqlite::types::Value = row.get(*name)?;
                    if &stored != expected {
                        return Ok(false);
                    }
                }
                Ok(true)
            },
        )
        .optional()?
        .unwrap_or(false))
}

impl RawDecision {
    fn decode_until(self, outcomes: Vec<OutcomeView>, deadline: Instant) -> Result<DecisionView, StoreError> {
        Ok(DecisionView {
            decision_id: DecisionId::parse(&self.id).map_err(|_| {
                rusqlite::Error::InvalidColumnType(0, "decision_id".to_string(), rusqlite::types::Type::Text)
            })?,
            prior_decision_id: self
                .prior_decision_id
                .map(|id| DecisionId::parse(&id))
                .transpose()
                .map_err(|_| {
                    rusqlite::Error::InvalidColumnType(27, "prior_decision_id".to_string(), rusqlite::types::Type::Text)
                })?,
            created_at_ms: self.created_at_ms,
            has_owner: self.has_owner,
            session_key: self.session_key,
            run_id: self.run_id,
            task_id: self.task_id,
            channel: self.channel,
            enabled_snapshot: self.enabled_snapshot,
            policy_version: self.policy_version,
            scorer_version: self.scorer_version,
            backend_kind: self.backend_kind,
            backend_locality: self.backend_locality,
            configured_model: self.configured_model,
            backend_endpoint: self.backend_endpoint,
            candidates: recovery_json(&self.candidates, deadline)?,
            distributions: recovery_json(&self.distributions, deadline)?,
            builtin_shadow: recovery_json(&self.shadow, deadline)?,
            selected_candidate_id: self.selected_candidate_id,
            directive_kind: self.directive_kind,
            fallback_order: recovery_json(&self.fallback_order, deadline)?,
            abstention_reason: self.abstention_reason,
            propensity: self.propensity,
            explored: self.explored,
            latency_ms: self.latency_ms,
            attempts: recovery_json(&self.attempts, deadline)?,
            error_class: self.error_class,
            cache_hit: self.cache_hit,
            legacy_v1: self.legacy_v1,
            outcomes,
        })
    }
}

const MAX_RECOVERY_DECISION_BYTES: i64 = 1024 * 1024;

thread_local! {
    static RECOVERY_READ_DEADLINE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

fn recovery_time_left(deadline: Instant) -> Result<std::time::Duration, StoreError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or(StoreError::RecoveryDeadline)
}

fn recovery_busy(_attempt: i32) -> bool {
    let deadline = RECOVERY_READ_DEADLINE.with(std::cell::Cell::get);
    if let Some(deadline) = deadline {
        if let Ok(left) = recovery_time_left(deadline) {
            std::thread::sleep(left.min(std::time::Duration::from_millis(5)));
            return Instant::now() < deadline;
        }
    }
    false
}

struct RecoveryReadBudget<'a> {
    connection: &'a Connection,
    previous: Option<Instant>,
}
impl<'a> RecoveryReadBudget<'a> {
    fn new(connection: &'a Connection, deadline: Instant) -> Result<Self, StoreError> {
        recovery_time_left(deadline)?;
        let previous = RECOVERY_READ_DEADLINE.with(|cell| cell.replace(Some(deadline)));
        let guard = Self { connection, previous };
        connection.busy_handler(Some(recovery_busy))?;
        connection.progress_handler(1_000, Some(move || Instant::now() >= deadline))?;
        Ok(guard)
    }
}
impl Drop for RecoveryReadBudget<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.connection.progress_handler(0, None::<fn() -> bool>) {
            tracing::error!(%error, "decision recovery progress handler reset failed");
        }
        if let Err(error) = crate::runtime::sqlite_pool::install_bounded_busy_handler(self.connection) {
            tracing::error!(%error, "decision recovery busy handler reset failed");
        }
        RECOVERY_READ_DEADLINE.with(|cell| cell.set(self.previous));
    }
}

struct RecoveryJsonReader<'a> {
    remaining: &'a [u8],
    deadline: Instant,
}
impl std::io::Read for RecoveryJsonReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "decision recovery decode deadline elapsed",
            ));
        }
        std::io::Read::read(&mut self.remaining, buffer)
    }
}
fn recovery_json<T: serde::de::DeserializeOwned>(text: &str, deadline: Instant) -> Result<T, StoreError> {
    recovery_time_left(deadline)?;
    let result = serde_json::from_reader(RecoveryJsonReader {
        remaining: text.as_bytes(),
        deadline,
    });
    recovery_time_left(deadline)?;
    Ok(result?)
}

fn recovery_scope_bounds(scope: &DecisionScope) -> Result<(), StoreError> {
    let (_, [workspace, owner]) = scope.clause();
    if workspace.is_empty() || workspace.len() > 4096 || owner.len() > 4096 {
        return Err(StoreError::RecoveryBounds);
    }
    Ok(())
}

fn recovery_schema(conn: &Connection, minimum: i64) -> Result<(), StoreError> {
    let version = store_schema::version(conn)?;
    if !(minimum..=store_schema::SCHEMA_VERSION).contains(&version) {
        return Err(StoreError::RecoverySchema);
    }
    Ok(())
}

fn recovery_size_sql<'a>(names: impl Iterator<Item = &'a str>, alias: &str) -> String {
    names
        .map(|name| format!("COALESCE(length(CAST({alias}.{name} AS BLOB)), 0)"))
        .collect::<Vec<_>>()
        .join(" + ")
}

/// Called only inside the recovery read snapshot. Measuring before loading
/// strings bounds every hashed column as well as the decoded projection.
fn replay_record_on_connection_until(
    conn: &Connection,
    scope: &DecisionScope,
    id: &DecisionId,
    deadline: Instant,
) -> Result<Option<DecisionReplayRecord>, StoreError> {
    recovery_time_left(deadline)?;
    recovery_schema(conn, 3)?;
    let size = recovery_size_sql(
        RECORD_HASH_COLUMNS
            .iter()
            .copied()
            .chain(["prior_decision_id", "backend_endpoint"]),
        "d",
    );
    let (clause, [first, second]) = scope.clause();
    let bytes: Option<i64> = conn
        .query_row(
            &format!("SELECT {size} FROM decisions d WHERE d.decision_id = ?1 AND {clause}"),
            params![id.to_string(), first, second],
            |row| row.get(0),
        )
        .optional()?;
    match bytes {
        None => return Ok(None),
        Some(bytes) if !(0..=MAX_RECOVERY_DECISION_BYTES).contains(&bytes) => {
            return Err(StoreError::RecoveryBounds);
        }
        Some(_) => {}
    }
    recovery_time_left(deadline)?;
    let result = replay_record_on_connection_with_deadline(conn, scope, id, Some(deadline))?;
    recovery_time_left(deadline)?;
    Ok(result)
}

fn outcome_matches_on_connection_until(
    conn: &Connection,
    scope: &DecisionScope,
    record: &OutcomeRecord,
    deadline: Instant,
) -> Result<bool, StoreError> {
    recovery_time_left(deadline)?;
    // This checks every vector/key/code bound before EncodedOutcome allocates
    // copies; it is the same validity contract enforced by the real writer.
    if !record.valid_for_queue() {
        return Err(StoreError::RecoveryBounds);
    }
    recovery_time_left(deadline)?;
    recovery_schema(conn, 1)?;
    let encoded = EncodedOutcome::of(record)?;
    let size = recovery_size_sql(encoded.columns.iter().map(|(name, _)| *name), "o");
    let (clause, [first, second]) = scope.clause();
    let filter = format!(
        "FROM outcomes o JOIN decisions d ON d.decision_id = o.decision_id
        WHERE d.decision_id = ?1 AND {clause} AND o.settlement_id = ?4"
    );
    let bytes: Option<i64> = conn
        .query_row(
            &format!("SELECT {size} {filter}"),
            params![record.decision_id.to_string(), first, second, record.settlement_id],
            |row| row.get(0),
        )
        .optional()?;
    match bytes {
        None => return Ok(false),
        Some(bytes) if !(0..=MAX_OUTCOME_STORED_BYTES as i64).contains(&bytes) => {
            return Err(StoreError::RecoveryBounds);
        }
        Some(_) => {}
    }
    recovery_time_left(deadline)?;
    let columns = encoded
        .columns
        .iter()
        .filter(|(name, _)| *name != "recorded_at_ms")
        .map(|(name, _)| format!("o.{name}"))
        .collect::<Vec<_>>()
        .join(", ");
    let matches = conn
        .query_row(
            &format!("SELECT {columns} {filter}"),
            params![record.decision_id.to_string(), first, second, record.settlement_id],
            |row| {
                for (name, expected) in encoded.columns.iter().filter(|(name, _)| *name != "recorded_at_ms") {
                    if row.get_ref(*name)? != rusqlite::types::ValueRef::from(expected) {
                        return Ok(false);
                    }
                }
                Ok(true)
            },
        )
        .optional()?
        .unwrap_or(false);
    recovery_time_left(deadline)?;
    Ok(matches)
}
