//! v1 feedback compatibility (SP-CDP-A 5.10, CDP-PLAN M14 and G13).
//!
//! This is the one decoder of the legacy CTE's JSONL feedback lines. It stays
//! after CDP-31 so old files remain readable; the files themselves are never
//! changed.
//!
//! The importer turns each line into a decision row with `legacy_v1` set and
//! no owner. The raw goal is **not** stored: the state fingerprint is a keyed
//! HMAC of the goal and the user intent. The branches become candidates (the
//! v1 label mapped to its action, no target). No probability is made up, and
//! no outcome is recorded. Malformed lines, and lines without a timestamp
//! (which retention would otherwise drop at once), are counted and skipped
//! with one warning per file.
//!
//! Files are read line by line with a bounded line length, never through a
//! symlink (D-091 F-11). Progress is kept per file (its digest and line
//! count) and saved only once the writer has stored every decision of the
//! file, so an unchanged file is not read again and a failed write is
//! retried on the next open. The ids hash each line's origin (file, line
//! number, line digest) under a fixed domain, not the workspace key, so a
//! re-import writes nothing new even after the key is regenerated.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::branch::{CapabilityTarget, CausalActionKind};
use super::ids::DecisionId;
use super::store::{
    CausalDecisionStore, DecisionRecord, Enqueue, ImportProgress, RecordLineage, RecordedCandidate,
    RecordedDistributions, RecordedShadow, StoreError,
};
use crate::decision::{DecisionKey, KeyDomain};

/// The longest v1 line read; a longer one is counted as malformed.
const MAX_LINE_BYTES: u64 = 1 << 20;

/// The domain of imported decision ids.
const ID_DOMAIN: &[u8] = b"prx.decision.v1_import_id.v1";

/// A v1 branch label (deserialization only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) enum V1Label {
    DirectAnswer,
    RetrieveThenAnswer,
    AskApproval,
}

impl V1Label {
    /// The action a v1 label corresponds to.
    pub(crate) const fn action(self) -> CausalActionKind {
        match self {
            Self::DirectAnswer => CausalActionKind::Answer,
            Self::RetrieveThenAnswer => CausalActionKind::Retrieve,
            Self::AskApproval => CausalActionKind::RequestApproval,
        }
    }
}

/// The tolerant form of a v1 line: every field optional.
#[derive(Debug, Deserialize)]
struct LegacyV1Record {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    goal: String,
    #[serde(default)]
    user_intent: String,
    #[serde(default)]
    chosen_branch_id: Option<String>,
    #[serde(default)]
    fallback_branch_ids: Vec<String>,
    #[serde(default)]
    branches: Vec<LegacyV1Branch>,
}

#[derive(Debug, Deserialize)]
struct LegacyV1Branch {
    branch_id: String,
    label: V1Label,
}

/// One decoded v1 decision. The goal and intent are kept in memory only, to
/// derive the keyed fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct V1Decision {
    /// `None` when the line has no RFC 3339 timestamp.
    pub(crate) created_at_ms: Option<i64>,
    pub(crate) session_id: Option<String>,
    pub(crate) request_id: Option<String>,
    pub(crate) goal: String,
    pub(crate) user_intent: String,
    pub(crate) chosen_branch_id: Option<String>,
    pub(crate) fallback_branch_ids: Vec<String>,
    pub(crate) branches: Vec<(String, V1Label)>,
}

/// Why a line is not a v1 decision.
#[derive(Debug, thiserror::Error)]
pub(crate) enum V1DecodeError {
    #[error("the line is empty")]
    Empty,
    #[error("the line is not a v1 feedback record: {0}")]
    Json(#[from] serde_json::Error),
}

/// Decode one v1 feedback line.
///
/// # Errors
///
/// [`V1DecodeError`] for an empty line or one that is not a v1 record.
pub(crate) fn decode_v1_feedback_line(line: &str) -> Result<V1Decision, V1DecodeError> {
    let line = line.trim();
    if line.is_empty() {
        return Err(V1DecodeError::Empty);
    }
    let record: LegacyV1Record = serde_json::from_str(line)?;
    let created_at_ms = record
        .timestamp
        .as_deref()
        .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|timestamp| timestamp.timestamp_millis());
    Ok(V1Decision {
        created_at_ms,
        session_id: record.session_id.filter(|value| !value.is_empty()),
        request_id: record.request_id.filter(|value| !value.is_empty()),
        goal: record.goal,
        user_intent: record.user_intent,
        chosen_branch_id: record.chosen_branch_id.filter(|value| !value.is_empty()),
        fallback_branch_ids: record.fallback_branch_ids,
        branches: record
            .branches
            .into_iter()
            .map(|branch| (branch.branch_id, branch.label))
            .collect(),
    })
}

/// The version fields of an imported row.
const LEGACY: &str = "legacy_v1";

/// Lines queued between two drains of the writer queue.
const PACE: usize = 256;

/// The digest of the fields of `decision` an imported id is made of: every
/// decoded field but the goal and the intent, in a fixed, length-prefixed
/// encoding written here (D-094 A-1), so no serializer setting (such as a
/// dependency turning on `serde_json`'s `preserve_order`) can change an id.
fn public_digest(decision: &V1Decision, created_at_ms: i64) -> [u8; 32] {
    fn bytes(digest: &mut Sha256, value: &[u8]) {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    fn optional(digest: &mut Sha256, value: Option<&str>) {
        match value {
            None => digest.update([0]),
            Some(value) => {
                digest.update([1]);
                bytes(digest, value.as_bytes());
            }
        }
    }
    let mut digest = Sha256::new();
    digest.update(created_at_ms.to_be_bytes());
    optional(&mut digest, decision.session_id.as_deref());
    optional(&mut digest, decision.request_id.as_deref());
    optional(&mut digest, decision.chosen_branch_id.as_deref());
    digest.update((decision.fallback_branch_ids.len() as u64).to_be_bytes());
    for id in &decision.fallback_branch_ids {
        bytes(&mut digest, id.as_bytes());
    }
    digest.update((decision.branches.len() as u64).to_be_bytes());
    for (id, label) in &decision.branches {
        bytes(&mut digest, id.as_bytes());
        bytes(&mut digest, label.action().as_str().as_bytes());
    }
    digest.finalize().into()
}

/// The row of the v1 `decision` found at line `line_no` of `file`.
fn legacy_record(
    decision: &V1Decision,
    created_at_ms: i64,
    key: &DecisionKey,
    file: &str,
    line_no: u64,
) -> DecisionRecord {
    // The id hashes the line's decoded fields without the goal and the
    // intent, so it is no guessing oracle for the goal; it stays
    // independent of the key (D-093 N-1).
    let public_digest = public_digest(decision, created_at_ms);
    let mut origin = Sha256::new();
    for part in [
        ID_DOMAIN,
        file.as_bytes(),
        &line_no.to_be_bytes()[..],
        &public_digest[..],
    ] {
        origin.update((part.len() as u64).to_be_bytes());
        origin.update(part);
    }
    let origin: [u8; 32] = origin.finalize().into();
    let fingerprint = key.hmac(
        KeyDomain::StateFingerprint,
        &[decision.goal.as_bytes(), decision.user_intent.as_bytes()],
    );
    let label_of = |id: &str| {
        decision
            .branches
            .iter()
            .find(|(branch, _)| branch == id)
            .map(|(_, label)| *label)
    };
    DecisionRecord {
        decision_id: DecisionId::legacy(&origin),
        created_at_ms,
        lineage: RecordLineage {
            // The writer stamps the store's own workspace id.
            workspace_id: Arc::from(""),
            owner_id: None,
            session_key: decision.session_id.as_deref().map(Arc::from),
            run_id: decision.request_id.as_deref().map(Arc::from),
            ..RecordLineage::default()
        },
        enabled_snapshot: true,
        state_fingerprint: hex::encode(fingerprint),
        redacted_projection: None,
        protocol_version: 0,
        policy_version: LEGACY.to_string(),
        catalog_generation: LEGACY.to_string(),
        scorer_version: LEGACY.to_string(),
        backend_kind: LEGACY.to_string(),
        backend_locality: "builtin".to_string(),
        configured_model: LEGACY.to_string(),
        backend_endpoint: None,
        claimed_model: None,
        provider_request_id: None,
        candidates: decision
            .branches
            .iter()
            .map(|(id, label)| RecordedCandidate {
                id: id.clone(),
                action: label.action().as_str().to_string(),
                target: CapabilityTarget::None.as_str().to_string(),
                propensity: None,
            })
            .collect(),
        gates: serde_json::json!([]),
        distributions: RecordedDistributions {
            legacy_v1: true,
            ..RecordedDistributions::default()
        },
        builtin_shadow: RecordedShadow::default(),
        features: serde_json::json!({}),
        selected_candidate_id: decision.chosen_branch_id.clone(),
        directive_kind: decision
            .chosen_branch_id
            .as_deref()
            .and_then(label_of)
            .map_or(LEGACY, |label| label.action().as_str())
            .to_string(),
        fallback_order: decision.fallback_branch_ids.clone(),
        abstention_reason: None,
        propensity: 1.0,
        explored: false,
        latency_ms: 0,
        usage: None,
        attempts: Vec::new(),
        error_class: None,
        cache_hit: false,
        response_sha256: None,
        legacy_v1: true,
    }
}

/// What an import did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ImportSummary {
    /// Files read.
    pub(crate) files: u64,
    /// Files skipped because they did not change since their import.
    pub(crate) unchanged: u64,
    /// Lines queued as decisions.
    pub(crate) imported: u64,
    /// Lines that were not v1 records.
    pub(crate) malformed: u64,
    /// Lines the writer queue dropped (the file is imported again later).
    pub(crate) dropped: u64,
    /// Lines without a timestamp, skipped.
    pub(crate) undated: u64,
}

/// Import `<workspace>/self/causal_tree/feedback/*.jsonl` into `store` as
/// legacy decisions. Does nothing on a read-only store. The files are only
/// read.
///
/// # Errors
///
/// [`StoreError`] when the feedback directory or a file cannot be read, or
/// the store's progress cannot be read.
pub(crate) async fn import_v1_feedback(
    store: &CausalDecisionStore,
    workspace_dir: &Path,
    key: &DecisionKey,
) -> Result<ImportSummary, StoreError> {
    let mut summary = ImportSummary::default();
    let Some(writer) = store.writer() else {
        return Ok(summary);
    };
    let dir = workspace_dir.join("self").join("causal_tree").join("feedback");
    match std::fs::symlink_metadata(&dir) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(StoreError::Unsafe(
                "the v1 feedback path is not a plain directory".to_string(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(summary),
        Err(error) => return Err(StoreError::Io(error)),
    }
    let mut files = std::fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
        .collect::<Vec<_>>();
    files.sort();
    let reader = store.reader();
    for path in files {
        let Some(file) = path.file_name().map(|name| name.to_string_lossy().to_string()) else {
            continue;
        };
        let (digest, total) = digest_file(&path)?;
        if reader
            .import_progress(&file)?
            .is_some_and(|progress| progress.digest == digest)
        {
            summary.unchanged += 1;
            continue;
        }
        summary.files += 1;
        let mut lines = LineReader::open(&path)?;
        let (mut line_no, mut malformed, mut undated, mut dropped) = (0_u64, 0_u64, 0_u64, 0_u64);
        let mut stored = Vec::new();
        while let Some(line) = lines.next_line()? {
            line_no += 1;
            let Line::Text(line) = line else {
                malformed += 1;
                continue;
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(decision) = decode_v1_feedback_line(&line) else {
                malformed += 1;
                continue;
            };
            let Some(created_at_ms) = decision.created_at_ms else {
                undated += 1;
                continue;
            };
            let record = legacy_record(&decision, created_at_ms, key, &file, line_no);
            let id = record.decision_id;
            match writer.record_decision(record) {
                Enqueue::Queued => {
                    summary.imported += 1;
                    stored.push(id);
                }
                Enqueue::Dropped => dropped += 1,
            }
            if summary.imported % PACE as u64 == 0 {
                writer.drain(Instant::now() + Duration::from_secs(5)).await;
            }
        }
        if malformed + undated > 0 {
            tracing::warn!(
                file = %file,
                malformed,
                undated,
                "v1 feedback lines that are not dated records were skipped"
            );
        }
        summary.malformed += malformed;
        summary.undated += undated;
        summary.dropped += dropped;
        // A file with dropped lines is not marked, and the writer marks a
        // file only once every queued decision is stored, so the next open
        // imports it again (the ids make that idempotent).
        if dropped == 0 {
            writer.mark_imported(
                ImportProgress {
                    file,
                    digest,
                    lines: total,
                },
                stored,
            );
        }
    }
    writer.drain(Instant::now() + Duration::from_secs(5)).await;
    Ok(summary)
}

/// Open a v1 file for reading without following a symlink; only a regular
/// file is read.
fn open_regular(path: &Path) -> Result<std::fs::File, StoreError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(StoreError::Unsafe(
            "a v1 feedback file is not a regular file".to_string(),
        ));
    }
    Ok(file)
}

/// The SHA-256 of a v1 file and its number of lines, read in blocks.
fn digest_file(path: &Path) -> Result<(String, u64), StoreError> {
    use std::io::Read as _;
    let mut file = open_regular(path)?;
    let mut hasher = Sha256::new();
    let mut block = [0_u8; 64 * 1024];
    let mut lines = 0_u64;
    loop {
        let read = file.read(&mut block)?;
        let Some(chunk) = block.get(..read).filter(|chunk| !chunk.is_empty()) else {
            break;
        };
        hasher.update(chunk);
        lines += chunk.iter().fold(0, |count, byte| count + u64::from(*byte == b'\n'));
    }
    Ok((hex::encode(hasher.finalize()), lines))
}

/// One line of a v1 file.
enum Line {
    Text(String),
    /// Longer than [`MAX_LINE_BYTES`]; skipped to its end.
    TooLong,
}

/// A v1 file read one bounded line at a time.
struct LineReader {
    reader: std::io::BufReader<std::fs::File>,
}

impl LineReader {
    fn open(path: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            reader: std::io::BufReader::new(open_regular(path)?),
        })
    }

    /// The next line, `None` at the end.
    fn next_line(&mut self) -> Result<Option<Line>, StoreError> {
        use std::io::{BufRead as _, Read as _};
        let mut line = Vec::new();
        let read = (&mut self.reader)
            .take(MAX_LINE_BYTES + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(None);
        }
        if u64::try_from(line.len()).unwrap_or(u64::MAX) > MAX_LINE_BYTES && !line.ends_with(b"\n") {
            // Skip the rest of the over-long line.
            let mut rest = Vec::new();
            loop {
                rest.clear();
                let more = (&mut self.reader).take(MAX_LINE_BYTES).read_until(b'\n', &mut rest)?;
                if more == 0 || rest.ends_with(b"\n") {
                    break;
                }
            }
            return Ok(Some(Line::TooLong));
        }
        let text = String::from_utf8_lossy(&line);
        Ok(Some(Line::Text(text.trim_end_matches(['\n', '\r']).to_string())))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::causal_tree::store::testing::open_private;
    use crate::causal_tree::store::{DecisionScope, StoreMode};
    use crate::decision::telemetry::testing::RecordingSink;

    const GOAL: &str = "GOAL-SENTINEL-73 please summarise my private notes";

    fn line(request: &str, label: &str) -> String {
        serde_json::json!({
            "timestamp": "2026-09-01T10:00:00Z",
            "session_id": "session-1",
            "request_id": request,
            "goal": GOAL,
            "user_intent": "simple",
            "chosen_branch_id": "b-1",
            "chosen_rank": 0,
            "fallback_branch_ids": ["b-2"],
            "rejected_branch_ids": [],
            "total_branches": 2,
            "cache_ttl_seconds": 30,
            "reasons": ["because"],
            "branches": [
                {"branch_id": "b-1", "label": label, "confidence": 0.9},
                {"branch_id": "b-2", "label": "DirectAnswer"}
            ]
        })
        .to_string()
    }

    fn feedback(workspace: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let dir = workspace.join("self").join("causal_tree").join("feedback");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    /// M14: the v1 labels map onto the action codes of the domain, and a
    /// line of the v1 writer decodes.
    #[test]
    fn v1_labels_decode_to_the_domain_action_codes() {
        let decoded = decode_v1_feedback_line(&line("r-1", "RetrieveThenAnswer")).unwrap();
        assert_eq!(decoded.branches[0], ("b-1".to_string(), V1Label::RetrieveThenAnswer));
        assert_eq!(decoded.created_at_ms, Some(1_788_256_800_000));
        assert_eq!(V1Label::DirectAnswer.action().as_str(), "answer");
        assert_eq!(V1Label::RetrieveThenAnswer.action().as_str(), "retrieve");
        assert_eq!(V1Label::AskApproval.action().as_str(), "request_approval");
        assert!(decode_v1_feedback_line("").is_err());
        assert!(decode_v1_feedback_line("{not json").is_err());
        assert!(decode_v1_feedback_line(r#"{"branches":[{"branch_id":"x","label":"Other"}]}"#).is_err());
    }

    /// V8.04 `no_decision_record_holds_raw_user_text`: after an import, no
    /// file of the store holds the goal; the fingerprint is keyed.
    #[tokio::test]
    async fn no_decision_record_holds_raw_user_text() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = DecisionKey::load_or_create(temp.path()).unwrap();
        feedback(temp.path(), "2026-09-01.jsonl", &line("r-1", "AskApproval"));
        let store = open_private(temp.path(), Arc::new(RecordingSink::default()));
        let summary = import_v1_feedback(&store, temp.path(), &key).await.unwrap();
        assert_eq!(summary.imported, 1);
        let view = store
            .reader()
            .evaluation_rows(
                &DecisionScope::LocalOperator {
                    workspace_id: store.workspace_id().unwrap(),
                },
                super::super::store::TimeWindow {
                    from_ms: 0,
                    to_ms: i64::MAX,
                },
            )
            .unwrap();
        assert_eq!(view.legacy_excluded, 1, "legacy rows are left out of evaluation");
        drop(store);
        let dir = temp.path().join("self").join("causal_tree");
        let mut read_database = false;
        for entry in std::fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("decisions.db") {
                // The writer thread may still be closing: its -wal and -shm
                // files can vanish after the listing (a checkpoint).
                let Ok(bytes) = std::fs::read(entry.path()) else {
                    assert_ne!(name, "decisions.db", "the database itself is read");
                    continue;
                };
                read_database |= name == "decisions.db";
                let text = String::from_utf8_lossy(&bytes);
                assert!(!text.contains("GOAL-SENTINEL-73"), "{name} holds the raw goal");
                let plain = hex::encode(Sha256::digest(format!("{GOAL}simple").as_bytes()));
                assert!(!text.contains(&plain), "{name} holds a plain hash of the goal");
            }
        }
        assert!(read_database);
    }

    /// Importing twice changes nothing and leaves the files as they were;
    /// the row is legacy, ownerless and carries no made-up probability.
    #[tokio::test]
    async fn v1_import_is_idempotent_and_keeps_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = DecisionKey::load_or_create(temp.path()).unwrap();
        let body = format!(
            "{}\nnot a record\n{}\n\n",
            line("r-1", "DirectAnswer"),
            line("r-2", "RetrieveThenAnswer")
        );
        let path = feedback(temp.path(), "2026-09-01.jsonl", &body);
        let store = open_private(temp.path(), Arc::new(RecordingSink::default()));
        let first = import_v1_feedback(&store, temp.path(), &key).await.unwrap();
        assert_eq!((first.files, first.imported, first.malformed), (1, 2, 1));
        let second = import_v1_feedback(&store, temp.path(), &key).await.unwrap();
        assert_eq!((second.files, second.unchanged, second.imported), (0, 1, 0));
        let counts = store.reader().counts().unwrap();
        assert_eq!((counts.decisions, counts.legacy_decisions, counts.outcomes), (2, 2, 0));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body, "the file is untouched");

        // A changed file is read again; its old lines are not duplicated.
        std::fs::write(&path, format!("{body}{}\n", line("r-3", "AskApproval"))).unwrap();
        let third = import_v1_feedback(&store, temp.path(), &key).await.unwrap();
        assert_eq!(third.files, 1);
        assert_eq!(store.reader().counts().unwrap().decisions, 3);

        let scope = DecisionScope::LocalOperator {
            workspace_id: store.workspace_id().unwrap(),
        };
        let conn = rusqlite::Connection::open(temp.path().join("self/causal_tree/decisions.db")).unwrap();
        let (owner, distributions): (Option<String>, String) = conn
            .query_row(
                "SELECT owner_id, distributions_json FROM decisions LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(owner, None);
        assert_eq!(distributions, r#"{"legacy_v1":true}"#);
        let id: String = conn
            .query_row("SELECT decision_id FROM decisions WHERE run_id = 'r-2'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let view = store
            .reader()
            .get(&scope, &DecisionId::parse(&id).unwrap())
            .unwrap()
            .unwrap();
        assert!(view.legacy_v1 && view.outcomes.is_empty());
        assert_eq!(view.directive_kind, "retrieve");
        assert_eq!(
            view.selected().map(|candidate| candidate.action.as_str()),
            Some("retrieve")
        );
    }

    /// A read-only store imports nothing.
    #[tokio::test]
    async fn a_read_only_store_imports_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = DecisionKey::load_or_create(temp.path()).unwrap();
        feedback(temp.path(), "2026-09-01.jsonl", &line("r-1", "DirectAnswer"));
        let store = CausalDecisionStore::open_shared(
            temp.path(),
            super::super::store::Retention::days(30, 30),
            Arc::new(RecordingSink::default()),
            StoreMode::ReadOnly,
        )
        .unwrap();
        assert_eq!(
            import_v1_feedback(&store, temp.path(), &key).await.unwrap(),
            ImportSummary::default()
        );
        assert!(!temp.path().join("self/causal_tree/decisions.db").exists());
    }

    /// D-091 F-11: a line without a timestamp is skipped and counted, never
    /// stored as created at 0 (which retention would drop at once).
    #[tokio::test]
    async fn an_undated_line_is_skipped_and_counted() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = DecisionKey::load_or_create(temp.path()).unwrap();
        let mut undated: serde_json::Value = serde_json::from_str(&line("r-1", "DirectAnswer")).unwrap();
        undated.as_object_mut().unwrap().remove("timestamp");
        feedback(
            temp.path(),
            "2026-09-01.jsonl",
            &format!("{undated}\n{}\n", line("r-2", "DirectAnswer")),
        );
        let store = open_private(temp.path(), Arc::new(RecordingSink::default()));
        let summary = import_v1_feedback(&store, temp.path(), &key).await.unwrap();
        assert_eq!((summary.imported, summary.undated), (1, 1));
        assert_eq!(store.reader().counts().unwrap().decisions, 1);
    }

    /// D-091 F-11: a symlinked v1 file is not read, and a symlinked feedback
    /// directory is refused.
    #[tokio::test]
    async fn symlinked_feedback_is_not_read() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = DecisionKey::load_or_create(temp.path()).unwrap();
        let outside = temp.path().join("outside.jsonl");
        std::fs::write(&outside, format!("{}\n", line("r-1", "DirectAnswer"))).unwrap();
        let dir = temp.path().join("self/causal_tree/feedback");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("2026-09-01.jsonl")).unwrap();
        let store = open_private(temp.path(), Arc::new(RecordingSink::default()));
        let summary = import_v1_feedback(&store, temp.path(), &key).await.unwrap();
        assert_eq!((summary.files, summary.imported), (0, 0));

        let other = tempfile::TempDir::new().unwrap();
        let key = DecisionKey::load_or_create(other.path()).unwrap();
        let real = other.path().join("real-feedback");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(
            real.join("2026-09-01.jsonl"),
            format!("{}\n", line("r-1", "DirectAnswer")),
        )
        .unwrap();
        std::os::unix::fs::symlink(&real, other.path().join("self/causal_tree/feedback")).unwrap();
        let store = open_private(other.path(), Arc::new(RecordingSink::default()));
        assert!(matches!(
            import_v1_feedback(&store, other.path(), &key).await,
            Err(StoreError::Unsafe(_))
        ));
    }

    /// D-091 F-11: the ids do not depend on the workspace key: after the key
    /// is regenerated, a changed file is imported again without duplicates.
    #[tokio::test]
    async fn ids_are_stable_across_key_regeneration() {
        let temp = tempfile::TempDir::new().unwrap();
        let body = format!("{}\n", line("r-1", "DirectAnswer"));
        let path = feedback(temp.path(), "2026-09-01.jsonl", &body);
        let store = open_private(temp.path(), Arc::new(RecordingSink::default()));
        let key = DecisionKey::load_or_create(temp.path()).unwrap();
        import_v1_feedback(&store, temp.path(), &key).await.unwrap();
        std::fs::remove_file(temp.path().join("self/causal_tree/.decision_key")).unwrap();
        let regenerated = DecisionKey::load_or_create(temp.path()).unwrap();
        std::fs::write(&path, format!("{body}{}\n", line("r-2", "AskApproval"))).unwrap();
        import_v1_feedback(&store, temp.path(), &regenerated).await.unwrap();
        assert_eq!(
            store.reader().counts().unwrap().decisions,
            2,
            "the first line is not imported twice"
        );
    }

    /// D-093 N-1: the id does not hash the goal or the intent: a line that
    /// differs only in them keeps its id.
    #[test]
    fn the_import_id_does_not_hash_the_goal() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = DecisionKey::load_or_create(temp.path()).unwrap();
        let first = decode_v1_feedback_line(&line("r-1", "DirectAnswer")).unwrap();
        let mut other_goal = first.clone();
        other_goal.goal = "a completely different goal".to_string();
        other_goal.user_intent = "delegate".to_string();
        let id = |decision: &V1Decision| legacy_record(decision, 1, &key, "f.jsonl", 1).decision_id;
        assert_eq!(id(&first), id(&other_goal));
        let mut other_request = first.clone();
        other_request.request_id = Some("r-2".to_string());
        assert_ne!(id(&first), id(&other_request));
    }

    /// D-094 A-1: the import id is a fixed encoding of the line's public
    /// fields, independent of any serializer's key order: this value is
    /// pinned, and an absent field differs from an empty one.
    #[test]
    fn the_import_id_is_a_fixed_encoding() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = DecisionKey::load_or_create(temp.path()).unwrap();
        let decision = V1Decision {
            created_at_ms: Some(1_767_225_600_000),
            session_id: Some("s-1".to_string()),
            request_id: None,
            goal: "a goal".to_string(),
            user_intent: "ask".to_string(),
            chosen_branch_id: Some("b-1".to_string()),
            fallback_branch_ids: vec!["b-2".to_string()],
            branches: vec![
                ("b-1".to_string(), V1Label::DirectAnswer),
                ("b-2".to_string(), V1Label::AskApproval),
            ],
        };
        let id = |decision: &V1Decision| {
            legacy_record(decision, 1_767_225_600_000, &key, "2026-01-01.jsonl", 3)
                .decision_id
                .to_string()
        };
        assert_eq!(id(&decision), "b9aac923-0795-84c6-9288-4bfbdbfea3f5");
        let mut empty = decision.clone();
        empty.request_id = Some(String::new());
        assert_ne!(id(&decision), id(&empty), "absent and empty differ");
        let mut split = decision.clone();
        split.fallback_branch_ids = vec!["b-2".to_string(), "b-3".to_string()];
        let mut joined = decision;
        joined.fallback_branch_ids = vec!["b-2b-3".to_string()];
        assert_ne!(id(&split), id(&joined), "list boundaries are encoded");
    }
}
