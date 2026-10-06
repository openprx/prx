//! Provider-neutral decision protocol types (CDP section 4.1, CDP-PLAN 1.2).
//!
//! The contract follows System One semantics: one state shared by every
//! question, caller-named questions that are answered independently, and
//! three primitives (Choice, Score, Noul). The wire mapping lives in
//! [`super::wire`]; the invariants of a response are checked by the response
//! validator (`validate.rs`).
//!
//! Identifiers are built only from typed parts ([`GlobalQuestion`],
//! [`CandidateQuestion`], [`CandidateKey`] and static option codes). Nothing
//! here turns an arbitrary string into a question, candidate or option id, so
//! no user text can name one (AG-19).
//!
//! Wire-format amendments from D-042 are applied here: probabilities are
//! `f64` (C-1), the Score legend is structured (C-5), and the limits are 2..=255
//! Choice options and 2..=10 Score levels (C-3).

use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use sha2::{Digest as _, Sha256};

use super::error::{DecisionError, RequestRule, ValidationRule};
use super::key::DecisionKey;
use crate::providers::ProviderId;

/// The decision protocol version PRX speaks.
pub const PROTOCOL_VERSION: u16 = 1;

/// The fewest options a Choice may offer.
pub const MIN_CHOICE_OPTIONS: usize = 2;
/// The most options a Choice may offer (the documented System One limit).
pub const MAX_CHOICE_OPTIONS: usize = 255;
/// The fewest levels a Score may have.
pub const MIN_SCORE_LEVELS: usize = 2;
/// The most levels a Score may have (the documented System One limit).
pub const MAX_SCORE_LEVELS: usize = 10;
/// The longest backend or model identity claim, in characters.
pub const MAX_CLAIM_CHARS: usize = 128;
/// The default request byte budget (`causal_tree.backend.max_request_bytes`).
pub const DEFAULT_MAX_REQUEST_BYTES: u32 = 64 * 1024;

/// The longest run of base64-alphabet characters without whitespace that a
/// structured string may carry. A longer run is treated as binary data.
const MAX_BASE64_RUN: usize = 1024;

/// Whether `text` is a well-formed identity string: a model name sent to a
/// backend, or a backend, model or request id a backend returns. It is not
/// blank, has at most [`MAX_CLAIM_CHARS`] characters, and carries no
/// character of the Unicode general categories Cc (controls), Cf (format:
/// bidirectional marks, zero-width characters, the byte-order mark), Zl or Zp
/// (line and paragraph separators), so it can be printed by the CLI, the
/// Console and logs without escaping and cannot hide a look-alike name
/// (D-057 M-4 and amendment 2, N-4).
#[must_use]
pub fn is_identity_text(text: &str) -> bool {
    !text.trim().is_empty()
        && text.chars().count() <= MAX_CLAIM_CHARS
        && !text.chars().any(|character| {
            character.is_control() || is_format_character(character) || matches!(character, '\u{2028}' | '\u{2029}')
        })
}

/// The Unicode general category Cf (format characters), as of Unicode 15.1.
/// The builtin's structure scan skips exactly this set (D-061 amendment).
pub(crate) const fn is_format_character(character: char) -> bool {
    matches!(
        character,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// A question every request may ask about the whole state (CDP section 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GlobalQuestion {
    /// Choice: the best legal action kind.
    NextAction,
    /// Choice: the best legal capability family.
    CapabilityTarget,
    /// Noul: the current evidence is sufficient.
    EvidenceSufficient,
    /// Noul: existing artifacts may be stale.
    NeedsFreshState,
    /// Score: predicted failure or harm risk.
    SemanticRisk,
    /// Score: expected task-success improvement.
    ExpectedGain,
}

impl GlobalQuestion {
    /// Every global question, in declaration order.
    pub const ALL: [Self; 6] = [
        Self::NextAction,
        Self::CapabilityTarget,
        Self::EvidenceSufficient,
        Self::NeedsFreshState,
        Self::SemanticRisk,
        Self::ExpectedGain,
    ];

    /// The question id.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NextAction => "next_action",
            Self::CapabilityTarget => "capability_target",
            Self::EvidenceSufficient => "evidence_sufficient",
            Self::NeedsFreshState => "needs_fresh_state",
            Self::SemanticRisk => "semantic_risk",
            Self::ExpectedGain => "expected_gain",
        }
    }
}

/// A question asked once per candidate (CDP section 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CandidateQuestion {
    /// Noul: the candidate applies to this state.
    Applicable,
    /// Noul: the candidate is likely to succeed.
    Success,
    /// Score: candidate-specific expected gain.
    Gain,
    /// Score: candidate-specific semantic risk.
    Risk,
}

impl CandidateQuestion {
    /// Every per-candidate question, in declaration order.
    pub const ALL: [Self; 4] = [Self::Applicable, Self::Success, Self::Gain, Self::Risk];

    /// The last segment of the question id.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applicable => "applicable",
            Self::Success => "success",
            Self::Gain => "gain",
            Self::Risk => "risk",
        }
    }
}

/// The id of one question in a request.
///
/// Grammar: `^[a-z][a-z0-9_]{0,31}(\.[a-z0-9][a-z0-9_-]{0,46}){0,2}$`, checked
/// by [`DecisionRequest::seal`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QuestionId(Arc<str>);

impl QuestionId {
    /// The id of a global question.
    #[must_use]
    pub fn global(question: GlobalQuestion) -> Self {
        Self(Arc::from(question.as_str()))
    }

    /// The id `candidate.<key>.<question>` of a per-candidate question.
    #[must_use]
    pub fn candidate(key: &CandidateKey, question: CandidateQuestion) -> Self {
        Self(Arc::from(format!("candidate.{}.{}", key.as_str(), question.as_str())))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_well_formed(&self) -> bool {
        let mut segments = self.0.split('.');
        let head_ok = segments.next().is_some_and(|head| {
            let mut bytes = head.bytes();
            bytes.next().is_some_and(|first| first.is_ascii_lowercase())
                && head.len() <= 32
                && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        });
        let tail: Vec<&str> = segments.collect();
        head_ok && tail.len() <= 2 && tail.iter().all(|segment| is_code_segment(segment))
    }
}

/// The stable key of one candidate inside question and option ids.
///
/// Grammar: `^[a-z0-9][a-z0-9_-]{0,46}$`, checked by [`DecisionRequest::seal`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CandidateKey(Arc<str>);

impl CandidateKey {
    /// The key `{action}-{target}[-{ns8}]`, where `ns8` is the hex form of a
    /// namespace discriminator (CDP-PLAN M17).
    #[must_use]
    pub fn from_parts(action: &'static str, target: &'static str, ns8: Option<[u8; 4]>) -> Self {
        let mut key = format!("{action}-{target}");
        if let Some(namespace) = ns8 {
            key.push('-');
            key.push_str(&hex::encode(namespace));
        }
        Self(Arc::from(key))
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The id of one Choice option. The option name is sent to the model, so it
/// is a readable PRX code (D-042 C-2).
///
/// Grammar: `^[a-z0-9][a-z0-9_-]{0,46}$`, checked by [`DecisionRequest::seal`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OptionId(Arc<str>);

impl OptionId {
    /// An option named by a static code (an action kind, a capability family,
    /// `other`, `none`).
    #[must_use]
    pub fn code(code: &'static str) -> Self {
        Self(Arc::from(code))
    }

    /// The option that names one candidate.
    #[must_use]
    pub fn candidate(key: &CandidateKey) -> Self {
        Self(Arc::clone(&key.0))
    }

    /// The option name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for OptionId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

fn is_code_segment(segment: &str) -> bool {
    let mut bytes = segment.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && segment.len() <= 47
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-')
}

// ---------------------------------------------------------------------------
// Structured values
// ---------------------------------------------------------------------------

/// Bounds on a [`StructuredValue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructuredLimits {
    /// The deepest nesting allowed; the top-level value is depth 1. Every
    /// value is enforced, 255 included (depth is counted in a wider type).
    pub max_depth: u8,
    /// The most JSON values (containers and leaves) allowed.
    pub max_nodes: u32,
    /// The longest string or object key, in bytes.
    pub max_string_bytes: u32,
}

impl StructuredLimits {
    /// The protocol defaults: depth 16, 4096 nodes, 8 KiB per string.
    pub const DEFAULT: Self = Self {
        max_depth: 16,
        max_nodes: 4096,
        max_string_bytes: 8 * 1024,
    };
}

impl Default for StructuredLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A validated JSON string, object or array: an instruction, a criterion, a
/// Score level or the request state.
///
/// It refuses a number, boolean or null at the top level, excessive nesting
/// or size, control characters, U+FFFD, long base64 runs (binary data) and
/// non-finite numbers. Nested `null` values are allowed.
///
/// `Debug` prints only the JSON kind: the value can hold user text (the
/// request state) and must never reach a log (invariant 16.10).
#[derive(Clone, PartialEq, Eq)]
pub struct StructuredValue(serde_json::Value);

impl fmt::Debug for StructuredValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match &self.0 {
            serde_json::Value::String(_) => "string",
            serde_json::Value::Object(_) => "object",
            serde_json::Value::Array(_) => "array",
            // Unreachable for a validated value; named rather than hidden.
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => "scalar",
        };
        write!(f, "StructuredValue(<{kind}>)")
    }
}

impl StructuredValue {
    /// Validate `value` against `limits`.
    pub fn with_limits(value: serde_json::Value, limits: &StructuredLimits) -> Result<Self, DecisionError> {
        check_structured(&value, limits)?;
        Ok(Self(value))
    }

    /// Re-check this value against (possibly stricter) `limits`.
    pub fn check(&self, limits: &StructuredLimits) -> Result<(), DecisionError> {
        check_structured(&self.0, limits)
    }

    /// The JSON value.
    #[must_use]
    pub const fn as_json(&self) -> &serde_json::Value {
        &self.0
    }

    /// Whether this value is a JSON object.
    #[must_use]
    pub fn is_object(&self) -> bool {
        self.0.is_object()
    }
}

impl TryFrom<serde_json::Value> for StructuredValue {
    type Error = DecisionError;

    /// Validate `value` against [`StructuredLimits::DEFAULT`].
    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        Self::with_limits(value, &StructuredLimits::DEFAULT)
    }
}

fn check_structured(value: &serde_json::Value, limits: &StructuredLimits) -> Result<(), DecisionError> {
    use serde_json::Value;

    let refuse = |rule| Err(DecisionError::InvalidRequest(rule));
    if !matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_)) {
        return refuse(RequestRule::StructuredShape);
    }
    if let Value::String(text) = value {
        if limits.max_nodes == 0 {
            return refuse(RequestRule::StructuredNodes);
        }
        if limits.max_depth == 0 {
            return refuse(RequestRule::StructuredDepth);
        }
        return check_text(text, limits);
    }
    let mut nodes = 0_u32;
    // Counted in `u16` so that no `max_depth` (not even 255) disables the
    // check: a `u8` count would saturate at 255 and never exceed it.
    let max_depth = u16::from(limits.max_depth);
    let mut pending: Vec<(&Value, u16)> = vec![(value, 1)];
    while let Some((node, depth)) = pending.pop() {
        nodes = nodes.saturating_add(1);
        if nodes > limits.max_nodes {
            return refuse(RequestRule::StructuredNodes);
        }
        if depth > max_depth {
            return refuse(RequestRule::StructuredDepth);
        }
        match node {
            Value::Null | Value::Bool(_) => {}
            Value::Number(number) => {
                if !number.as_f64().is_some_and(f64::is_finite) {
                    return refuse(RequestRule::StructuredNumber);
                }
            }
            Value::String(text) => check_text(text, limits)?,
            Value::Array(items) => pending.extend(items.iter().map(|item| (item, depth.saturating_add(1)))),
            Value::Object(map) => {
                for (key, item) in map {
                    check_text(key, limits)?;
                    pending.push((item, depth.saturating_add(1)));
                }
            }
        }
    }
    Ok(())
}

fn check_text(text: &str, limits: &StructuredLimits) -> Result<(), DecisionError> {
    if text.len() > limits.max_string_bytes as usize {
        return Err(DecisionError::InvalidRequest(RequestRule::StructuredStringLength));
    }
    let mut base64_run = 0_usize;
    for character in text.chars() {
        let control = character < ' ' && !matches!(character, '\t' | '\n' | '\r');
        if control || character == '\u{FFFD}' {
            return Err(DecisionError::InvalidRequest(RequestRule::StructuredText));
        }
        if character.is_ascii_alphanumeric() || matches!(character, '+' | '/' | '=' | '-' | '_') {
            base64_run += 1;
            if base64_run > MAX_BASE64_RUN {
                return Err(DecisionError::InvalidRequest(RequestRule::StructuredText));
            }
        } else {
            base64_run = 0;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Questions and requests
// ---------------------------------------------------------------------------

/// The optional descriptions of a Noul question's two outcomes. At least one
/// is present; PRX never sends `null` (DIFFS D-05).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoulCriteria {
    /// What "true" means (wire key `true`).
    pub when_true: Option<StructuredValue>,
    /// What "false" means (wire key `false`).
    pub when_false: Option<StructuredValue>,
}

/// The primitive a question asks for and an answer carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Primitive {
    /// A probability distribution over named options.
    Choice,
    /// A probability distribution over ordered levels.
    Score,
    /// The probability that a proposition is true.
    Noul,
}

impl Primitive {
    /// The wire `type` tag.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Choice => "choice",
            Self::Score => "score",
            Self::Noul => "noul",
        }
    }
}

/// One question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionQuestion {
    /// Pick one of the named options; every option has a description.
    Choice {
        /// What to decide.
        instructions: StructuredValue,
        /// Option name to description.
        criteria: BTreeMap<OptionId, StructuredValue>,
    },
    /// Place the state on ordered levels (wire name `criteria`).
    Score {
        /// What to rate.
        instructions: StructuredValue,
        /// The level descriptions, lowest first.
        levels: Vec<StructuredValue>,
    },
    /// Is the proposition true?
    Noul {
        /// The proposition.
        instructions: StructuredValue,
        /// Optional descriptions of the two outcomes.
        criteria: Option<NoulCriteria>,
    },
}

impl DecisionQuestion {
    /// The primitive this question asks for.
    #[must_use]
    pub const fn primitive(&self) -> Primitive {
        match self {
            Self::Choice { .. } => Primitive::Choice,
            Self::Score { .. } => Primitive::Score,
            Self::Noul { .. } => Primitive::Noul,
        }
    }

    /// The instructions.
    #[must_use]
    pub const fn instructions(&self) -> &StructuredValue {
        match self {
            Self::Choice { instructions, .. } | Self::Score { instructions, .. } | Self::Noul { instructions, .. } => {
                instructions
            }
        }
    }
}

/// Why a question the compiler could have asked was left out (CDP section 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OmissionReason {
    /// Only one legal non-terminal candidate exists, so the choice is forced.
    SingleLegalCandidate,
    /// No artifact exists, so nothing can be stale.
    NoArtifacts,
    /// Every candidate shares one capability target.
    SingleTarget,
    /// The candidate's outcome is determined by code (refusal,
    /// clarification).
    DeterministicCandidate,
}

impl OmissionReason {
    /// The stable reason code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SingleLegalCandidate => "single_legal_candidate",
            Self::NoArtifacts => "no_artifacts",
            Self::SingleTarget => "single_target",
            Self::DeterministicCandidate => "deterministic_candidate",
        }
    }
}

/// Everything a request is built from. [`DecisionRequest::seal`] is the only
/// way to turn parts into a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionAudit {
    /// Redacted projection fields cut to a bound, in deterministic order.
    pub truncated_fields: Vec<String>,
    /// Number of projected text values changed by redaction.
    pub redacted_texts: u32,
}

#[derive(Debug, Clone)]
pub struct RequestParts {
    /// The redacted state v1 object shared by every question.
    pub state: StructuredValue,
    /// The questions, keyed and ordered by id.
    pub questions: BTreeMap<QuestionId, DecisionQuestion>,
    /// The model to ask for; required by the wire format of HTTP backends.
    pub model: Option<String>,
    /// The keyed fingerprint of the causal state the request was built from.
    pub state_fingerprint: String,
    /// The hash of the legal candidate set.
    pub candidate_set_hash: String,
    /// The policy version in force.
    pub policy_version: Arc<str>,
    /// The capability catalog generation.
    pub catalog_generation: String,
    /// Questions deliberately left out, with the reason.
    pub omitted_questions: Vec<(QuestionId, OmissionReason)>,
    /// Whether an approval is pending for this turn; such a decision is never
    /// cached (CDP section 17).
    pub approval_pending: bool,
}

/// Bounds a request must fit before it may be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestLimits {
    /// Bounds on every structured value.
    pub structured: StructuredLimits,
    /// The largest canonical request, in bytes.
    pub max_request_bytes: u32,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            structured: StructuredLimits::DEFAULT,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
        }
    }
}

/// The local record of a sealed request. It is never sent on the wire; the
/// validator reads it from the request it validates against (AG-23). It is
/// read-only: every field is set by [`DecisionRequest::seal`] and exposed
/// through an accessor (D-057 M-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRequestManifest {
    request_id: String,
    request_fingerprint: [u8; 32],
    state_fingerprint: String,
    candidate_set_hash: String,
    policy_version: Arc<str>,
    catalog_generation: String,
    omitted_questions: Vec<(QuestionId, OmissionReason)>,
    projection_audit: ProjectionAudit,
    cacheable: bool,
    expires_at: Instant,
}

impl DecisionRequestManifest {
    /// The request id (UUIDv7), equal to [`DecisionRequest::request_id`].
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// SHA-256 over the canonical request content and its bindings (state
    /// fingerprint, candidate set, policy, catalog, omissions). Identical
    /// content gives an identical fingerprint; it keys wire salting and the
    /// decision cache.
    #[must_use]
    pub const fn request_fingerprint(&self) -> &[u8; 32] {
        &self.request_fingerprint
    }

    /// See [`RequestParts::state_fingerprint`].
    #[must_use]
    pub fn state_fingerprint(&self) -> &str {
        &self.state_fingerprint
    }

    /// See [`RequestParts::candidate_set_hash`].
    #[must_use]
    pub fn candidate_set_hash(&self) -> &str {
        &self.candidate_set_hash
    }

    /// See [`RequestParts::policy_version`].
    #[must_use]
    pub const fn policy_version(&self) -> &Arc<str> {
        &self.policy_version
    }

    /// See [`RequestParts::catalog_generation`].
    #[must_use]
    pub fn catalog_generation(&self) -> &str {
        &self.catalog_generation
    }

    /// See [`RequestParts::omitted_questions`].
    #[must_use]
    pub fn omitted_questions(&self) -> &[(QuestionId, OmissionReason)] {
        &self.omitted_questions
    }

    /// Redaction and truncation evidence for the state v1 projection.
    #[must_use]
    pub const fn projection_audit(&self) -> &ProjectionAudit {
        &self.projection_audit
    }

    /// False whenever an approval is pending (CDP section 17).
    #[must_use]
    pub const fn cacheable(&self) -> bool {
        self.cacheable
    }

    /// The total decision deadline: the one source of truth for every stage.
    #[must_use]
    pub const fn expires_at(&self) -> Instant {
        self.expires_at
    }
}

/// A sealed, immutable decision request.
///
/// Every sealing entry validates before private construction, and no field
/// can be changed afterwards (D-057 M-1): the content, the manifest and the
/// fingerprint always agree. There is no `Clone`: one sealed request is one
/// decision, and it owns the ledger of the responses already validated
/// against it.
///
/// `Debug` prints the request id, the protocol version, the question count
/// and the fingerprint, never the state or question text (D-057 M-5).
pub struct DecisionRequest {
    protocol_version: u16,
    request_id: String,
    state: StructuredValue,
    questions: BTreeMap<QuestionId, DecisionQuestion>,
    model: Option<String>,
    structured_limits: StructuredLimits,
    manifest: DecisionRequestManifest,
    /// The delivery digest (canonical decoded content) of every wire
    /// response already validated against this request; the same content
    /// delivered again, in any encoding, is refused (duplicate delivery).
    delivered: Mutex<BTreeSet<[u8; 32]>>,
}

impl fmt::Debug for DecisionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecisionRequest")
            .field("request_id", &self.request_id)
            .field("protocol_version", &self.protocol_version)
            .field("questions", &self.questions.len())
            .field("request_fingerprint", &hex::encode(self.manifest.request_fingerprint))
            .finish_non_exhaustive()
    }
}

/// Two requests are equal when their content and manifests are; the
/// delivery ledger is runtime state, not content.
impl PartialEq for DecisionRequest {
    fn eq(&self, other: &Self) -> bool {
        self.protocol_version == other.protocol_version
            && self.request_id == other.request_id
            && self.state == other.state
            && self.questions == other.questions
            && self.model == other.model
            && self.structured_limits == other.structured_limits
            && self.manifest == other.manifest
    }
}

impl Eq for DecisionRequest {}

impl DecisionRequest {
    /// Validate `parts` and seal them into a request that expires at
    /// `expires_at`.
    ///
    /// Before any transport exists, this checks: the state is an object;
    /// there is at least one question; every id matches its grammar; every
    /// structured value fits `limits.structured`; Choice has
    /// [`MIN_CHOICE_OPTIONS`]..=[`MAX_CHOICE_OPTIONS`] options and Score
    /// [`MIN_SCORE_LEVELS`]..=[`MAX_SCORE_LEVELS`] levels; a Noul criteria
    /// value names at least one outcome; the model name is an identity
    /// string ([`is_identity_text`]); no omitted question is also asked; the
    /// canonical content fits `limits.max_request_bytes`; and the salted wire
    /// ids of `key` do not collide; and a state `candidates` list names no id
    /// twice. It then assigns a UUIDv7 request id and computes the request
    /// fingerprint.
    pub fn seal(
        parts: RequestParts,
        key: &DecisionKey,
        expires_at: Instant,
        limits: &RequestLimits,
    ) -> Result<Self, DecisionError> {
        Self::seal_with_projection_audit(parts, ProjectionAudit::default(), key, expires_at, limits)
    }

    /// Seal a projected causal request with local redaction and truncation
    /// evidence. The original `seal` API keeps its legacy request bindings.
    pub fn seal_with_projection_audit(
        parts: RequestParts,
        projection_audit: ProjectionAudit,
        key: &DecisionKey,
        expires_at: Instant,
        limits: &RequestLimits,
    ) -> Result<Self, DecisionError> {
        let fingerprint = Self::validate_parts(&parts, &projection_audit, key, limits)?;
        Ok(Self::from_validated_parts(
            parts,
            projection_audit,
            fingerprint,
            expires_at,
            limits,
        ))
    }

    /// Recover original compiler ownership on failure without cloning a
    /// successful request. Recovery carries no sealed request or authority.
    pub(crate) fn seal_recovering_parts(
        parts: RequestParts,
        projection_audit: ProjectionAudit,
        key: &DecisionKey,
        expires_at: Instant,
        limits: &RequestLimits,
    ) -> Result<Self, (DecisionError, Box<RequestParts>)> {
        match Self::validate_parts(&parts, &projection_audit, key, limits) {
            Ok(fingerprint) => Ok(Self::from_validated_parts(
                parts,
                projection_audit,
                fingerprint,
                expires_at,
                limits,
            )),
            Err(error) => Err((error, Box::new(parts))),
        }
    }

    fn validate_parts(
        parts: &RequestParts,
        projection_audit: &ProjectionAudit,
        key: &DecisionKey,
        limits: &RequestLimits,
    ) -> Result<[u8; 32], DecisionError> {
        let refuse = |rule| DecisionError::InvalidRequest(rule);
        if !parts.state.is_object() {
            return Err(refuse(RequestRule::StateShape));
        }
        parts.state.check(&limits.structured)?;
        if has_duplicate_candidate_ids(parts.state.as_json()) {
            return Err(refuse(RequestRule::DuplicateCandidateId));
        }
        if parts.questions.is_empty() {
            return Err(refuse(RequestRule::NoQuestions));
        }
        for (id, question) in &parts.questions {
            if !id.is_well_formed() {
                return Err(refuse(RequestRule::IdentifierGrammar));
            }
            check_question(question, &limits.structured)?;
        }
        if let Some(model) = &parts.model {
            if !is_identity_text(model) {
                return Err(refuse(RequestRule::ModelInvalid));
            }
        }
        for (id, _) in &parts.omitted_questions {
            if !id.is_well_formed() {
                return Err(refuse(RequestRule::IdentifierGrammar));
            }
            if parts.questions.contains_key(id) {
                return Err(refuse(RequestRule::OmittedQuestionAsked));
            }
        }

        let content = canonical_content(parts);
        if content.len() > limits.max_request_bytes as usize {
            return Err(refuse(RequestRule::RequestTooLarge));
        }
        let mut hasher = Sha256::new();
        hasher.update(b"prx.decision.request.v1\0");
        hasher.update(content.as_bytes());
        hasher.update(b"\0");
        hasher.update(canonical_bindings(parts, projection_audit).as_bytes());
        let request_fingerprint: [u8; 32] = hasher.finalize().into();
        super::wire::salted_question_ids(key, &request_fingerprint, parts.questions.keys())?;

        Ok(request_fingerprint)
    }

    fn from_validated_parts(
        parts: RequestParts,
        projection_audit: ProjectionAudit,
        request_fingerprint: [u8; 32],
        expires_at: Instant,
        limits: &RequestLimits,
    ) -> Self {
        let request_id = uuid::Uuid::now_v7().to_string();
        let RequestParts {
            state,
            questions,
            model,
            state_fingerprint,
            candidate_set_hash,
            policy_version,
            catalog_generation,
            omitted_questions,
            approval_pending,
        } = parts;
        Self {
            protocol_version: PROTOCOL_VERSION,
            request_id: request_id.clone(),
            state,
            questions,
            model,
            structured_limits: limits.structured,
            manifest: DecisionRequestManifest {
                request_id,
                request_fingerprint,
                state_fingerprint,
                candidate_set_hash,
                policy_version,
                catalog_generation,
                omitted_questions,
                projection_audit,
                cacheable: !approval_pending,
                expires_at,
            },
            delivered: Mutex::new(BTreeSet::new()),
        }
    }

    /// Always [`PROTOCOL_VERSION`].
    #[must_use]
    pub const fn protocol_version(&self) -> u16 {
        self.protocol_version
    }

    /// The request id (UUIDv7); never sent on the wire.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// The redacted state object.
    #[must_use]
    pub const fn state(&self) -> &StructuredValue {
        &self.state
    }

    /// The questions, keyed and ordered by id.
    #[must_use]
    pub const fn questions(&self) -> &BTreeMap<QuestionId, DecisionQuestion> {
        &self.questions
    }

    /// The model to ask for.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The structured-value limits the request was sealed with; a Score
    /// legend echo is decoded under the same limits (D-057 L-9).
    #[must_use]
    pub const fn structured_limits(&self) -> &StructuredLimits {
        &self.structured_limits
    }

    /// The local manifest; never on the wire.
    #[must_use]
    pub const fn manifest(&self) -> &DecisionRequestManifest {
        &self.manifest
    }

    /// Record that a wire response with delivery digest `digest` (the
    /// canonical form of its decoded content) was validated against this
    /// request. False when the same content was recorded before: a duplicate
    /// delivery (SP-CDP-C T6), however it was re-encoded.
    pub(super) fn record_delivery(&self, digest: [u8; 32]) -> bool {
        self.delivered.lock().insert(digest)
    }
}

/// Whether the state's `candidates` list (state v1) names one id twice.
/// Every question and answer about a candidate is keyed by its id, so a
/// repeated id would let list order decide which candidate an answer is
/// about (D-061 L-2).
fn has_duplicate_candidate_ids(state: &serde_json::Value) -> bool {
    let Some(candidates) = state.get("candidates").and_then(serde_json::Value::as_array) else {
        return false;
    };
    let mut seen = BTreeSet::new();
    candidates
        .iter()
        .filter_map(|candidate| candidate.get("id").and_then(serde_json::Value::as_str))
        .any(|id| !seen.insert(id))
}

fn check_question(question: &DecisionQuestion, limits: &StructuredLimits) -> Result<(), DecisionError> {
    let refuse = |rule| Err(DecisionError::InvalidRequest(rule));
    question.instructions().check(limits)?;
    match question {
        DecisionQuestion::Choice { criteria, .. } => {
            if criteria.len() < MIN_CHOICE_OPTIONS {
                return refuse(RequestRule::MinChoiceOptions);
            }
            if criteria.len() > MAX_CHOICE_OPTIONS {
                return refuse(RequestRule::MaxChoiceOptions);
            }
            for (option, description) in criteria {
                if !is_code_segment(option.as_str()) {
                    return refuse(RequestRule::IdentifierGrammar);
                }
                description.check(limits)?;
            }
        }
        DecisionQuestion::Score { levels, .. } => {
            if levels.len() < MIN_SCORE_LEVELS {
                return refuse(RequestRule::MinScoreLevels);
            }
            if levels.len() > MAX_SCORE_LEVELS {
                return refuse(RequestRule::MaxScoreLevels);
            }
            for level in levels {
                level.check(limits)?;
            }
        }
        DecisionQuestion::Noul { criteria, .. } => {
            if let Some(criteria) = criteria {
                if criteria.when_true.is_none() && criteria.when_false.is_none() {
                    return refuse(RequestRule::NoulCriteriaShape);
                }
                for outcome in [&criteria.when_true, &criteria.when_false].into_iter().flatten() {
                    outcome.check(limits)?;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Canonical JSON
// ---------------------------------------------------------------------------

/// Write `value` as compact JSON with object keys in byte order: the same
/// bytes `serde_json` writes for a key-sorted value, independent of whether
/// its maps preserve insertion order.
pub(super) fn write_canonical(value: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_json_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                write_canonical(item, out);
            }
            out.push('}');
        }
    }
}

/// Write `text` as a JSON string literal, escaping exactly what `serde_json`
/// escapes.
pub(super) fn write_json_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            control if control < ' ' => {
                // Writing into a `String` cannot fail.
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Write one question in its wire shape: `{"criteria":…,"instructions":…,"type":…}`
/// with keys in byte order and the Noul `criteria` omitted when absent.
pub(super) fn write_question(question: &DecisionQuestion, out: &mut String) {
    out.push('{');
    match question {
        DecisionQuestion::Choice { criteria, .. } => {
            out.push_str("\"criteria\":{");
            for (index, (option, description)) in criteria.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json_string(option.as_str(), out);
                out.push(':');
                write_canonical(description.as_json(), out);
            }
            out.push_str("},");
        }
        DecisionQuestion::Score { levels, .. } => {
            out.push_str("\"criteria\":[");
            for (index, level) in levels.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(level.as_json(), out);
            }
            out.push_str("],");
        }
        DecisionQuestion::Noul { criteria, .. } => {
            if let Some(criteria) = criteria {
                out.push_str("\"criteria\":{");
                let outcomes = [("false", &criteria.when_false), ("true", &criteria.when_true)];
                let mut first = true;
                for (name, outcome) in outcomes {
                    if let Some(outcome) = outcome {
                        if !first {
                            out.push(',');
                        }
                        first = false;
                        write_json_string(name, out);
                        out.push(':');
                        write_canonical(outcome.as_json(), out);
                    }
                }
                out.push_str("},");
            }
        }
    }
    out.push_str("\"instructions\":");
    write_canonical(question.instructions().as_json(), out);
    out.push_str(",\"type\":");
    write_json_string(question.primitive().as_str(), out);
    out.push('}');
}

/// The canonical request content: model, protocol version, questions under
/// their PRX ids, and state.
fn canonical_content(parts: &RequestParts) -> String {
    let mut out = String::new();
    out.push_str("{\"model\":");
    match &parts.model {
        Some(model) => write_json_string(model, &mut out),
        None => out.push_str("null"),
    }
    let _ = write!(out, ",\"protocol_version\":{PROTOCOL_VERSION},\"questions\":{{");
    for (index, (id, question)) in parts.questions.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_json_string(id.as_str(), &mut out);
        out.push(':');
        write_question(question, &mut out);
    }
    out.push_str("},\"state\":");
    write_canonical(parts.state.as_json(), &mut out);
    out.push('}');
    out
}

/// The canonical bindings that tie a request to its trusted context.
fn canonical_bindings(parts: &RequestParts, projection_audit: &ProjectionAudit) -> String {
    let mut out = String::from("{\"candidate_set_hash\":");
    write_json_string(&parts.candidate_set_hash, &mut out);
    out.push_str(",\"catalog_generation\":");
    write_json_string(&parts.catalog_generation, &mut out);
    out.push_str(",\"omitted_questions\":[");
    for (index, (id, reason)) in parts.omitted_questions.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('[');
        write_json_string(id.as_str(), &mut out);
        out.push(',');
        write_json_string(reason.as_str(), &mut out);
        out.push(']');
    }
    out.push_str("],\"policy_version\":");
    write_json_string(&parts.policy_version, &mut out);
    // Existing requests carry no projection audit. Preserve their canonical
    // bindings and salted wire ids; only a nonempty CDP projection adds data.
    if *projection_audit != ProjectionAudit::default() {
        out.push_str(",\"projection_audit\":{\"redacted_texts\":");
        out.push_str(&projection_audit.redacted_texts.to_string());
        out.push_str(",\"truncated_fields\":[");
        for (index, field) in projection_audit.truncated_fields.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            write_json_string(field, &mut out);
        }
        out.push_str("]}");
    }
    out.push_str(",\"state_fingerprint\":");
    write_json_string(&parts.state_fingerprint, &mut out);
    out.push('}');
    out
}

// ---------------------------------------------------------------------------
// Answers and responses
// ---------------------------------------------------------------------------

/// A probability: a finite value in `[0, 1]`. The value is kept exactly as
/// received (D-042 C-1: `f64`, never repaired).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Probability(f64);

impl Probability {
    /// Accept `value` when it is finite and in `[0, 1]`.
    pub fn new(value: f64) -> Result<Self, DecisionError> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(DecisionError::InvalidResponse(ValidationRule::ProbabilityRange))
        }
    }

    /// The value.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// A Choice answer.
#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceAnswer {
    /// The chosen option (a highest-probability option).
    pub chosen: OptionId,
    /// The probability of every offered option.
    pub probabilities: BTreeMap<OptionId, Probability>,
    /// Aggregate confidence of the distribution: an abstention signal, not a
    /// per-option feature (CDP section 8).
    pub confidence: Probability,
}

/// A Score answer.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreAnswer {
    /// The probability of each level, lowest first.
    pub distribution: Vec<Probability>,
    /// The expected level index, in `[0, n-1]` (wire name `score`).
    pub expected: f64,
    /// The levels as echoed by the backend; equal to the request levels.
    pub legend: Vec<StructuredValue>,
    /// Aggregate confidence of the distribution.
    pub confidence: Probability,
}

/// A Noul answer.
#[derive(Debug, Clone, PartialEq)]
pub struct NoulAnswer {
    /// The probability that the proposition is true (wire name `noul`).
    pub probability_true: Probability,
}

/// One answer.
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionAnswer {
    /// Answer to a Choice question.
    Choice(ChoiceAnswer),
    /// Answer to a Score question.
    Score(ScoreAnswer),
    /// Answer to a Noul question.
    Noul(NoulAnswer),
}

impl DecisionAnswer {
    /// The primitive this answer carries.
    #[must_use]
    pub const fn primitive(&self) -> Primitive {
        match self {
            Self::Choice(_) => Primitive::Choice,
            Self::Score(_) => Primitive::Score,
            Self::Noul(_) => Primitive::Noul,
        }
    }
}

/// Token and cost accounting reported by a backend. Accounting only: a
/// missing value is never a validation failure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecisionUsage {
    /// Input tokens.
    pub input_tokens: Option<u32>,
    /// Output tokens.
    pub output_tokens: Option<u32>,
    /// Cost in millionths of a US dollar, when the backend reports one.
    pub cost_micro_usd: Option<u64>,
}

/// A decoded, not yet validated, backend response.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionResponse {
    /// The backend's name for itself: a claim, at most 128 characters, never
    /// a label or audit truth.
    pub backend: String,
    /// The model the backend claims answered: a claim, at most 128
    /// characters.
    pub resolved_model: String,
    /// The answers by question id. Duplicate keys are refused at decode.
    pub answers: BTreeMap<QuestionId, DecisionAnswer>,
    /// Usage accounting.
    pub usage: DecisionUsage,
    /// The provider's request id, a diagnostic token.
    pub provider_request_id: Option<String>,
    /// Time from send to decoded answer.
    pub elapsed_ms: u64,
    /// Size of the raw body; 0 for a backend without a wire body.
    pub raw_bytes: u32,
    /// SHA-256 of the raw body (all zero when `raw_bytes` is 0).
    pub response_sha256: [u8; 32],
}

/// The kind of backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// The local deterministic backend.
    Builtin,
    /// Any System One compatible HTTP endpoint.
    CompatibleHttp,
    /// The hosted TypeSafe adapter.
    TypeSafe,
    /// The hosted OpenRouter Decisions adapter.
    OpenRouter,
    /// The scripted test backend.
    #[cfg(any(test, feature = "test-mock"))]
    Mock,
}

impl BackendKind {
    /// The stable kind name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::CompatibleHttp => "compatible_http",
            Self::TypeSafe => "typesafe",
            Self::OpenRouter => "openrouter",
            #[cfg(any(test, feature = "test-mock"))]
            Self::Mock => "mock",
        }
    }
}

/// Where a backend runs. It is derived from the backend kind and the literal
/// loopback predicate, never from a URL alone for hosted kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendLocality {
    /// In process.
    Builtin,
    /// On a literal loopback address.
    Loopback,
    /// Anywhere else.
    Remote,
}

impl BackendLocality {
    /// The stable locality name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Loopback => "loopback",
            Self::Remote => "remote",
        }
    }
}

/// The locally configured identity of a backend: the audit truth, as opposed
/// to the claims in a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendIdentity {
    /// The backend kind.
    pub kind: BackendKind,
    /// The provider the backend belongs to, for hosted kinds.
    pub provider: Option<ProviderId>,
    /// The configured model.
    pub configured_model: Arc<str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> StructuredValue {
        StructuredValue::try_from(serde_json::Value::from(value)).expect("test: text")
    }

    #[test]
    fn default_projection_audit_preserves_legacy_binding_and_nondefault_is_sealed() {
        let parts = RequestParts {
            state: StructuredValue::try_from(serde_json::json!({"schema": "test"})).unwrap(),
            questions: BTreeMap::from([(
                QuestionId::global(GlobalQuestion::EvidenceSufficient),
                DecisionQuestion::Noul {
                    instructions: text("Is evidence sufficient?"),
                    criteria: None,
                },
            )]),
            model: None,
            state_fingerprint: "s".into(),
            candidate_set_hash: "c".into(),
            policy_version: Arc::from("p"),
            catalog_generation: "g".into(),
            omitted_questions: Vec::new(),
            approval_pending: false,
        };
        assert_eq!(
            canonical_bindings(&parts, &ProjectionAudit::default()),
            "{\"candidate_set_hash\":\"c\",\"catalog_generation\":\"g\",\"omitted_questions\":[],\"policy_version\":\"p\",\"state_fingerprint\":\"s\"}"
        );
        let key = crate::causal_tree::state::fixtures::key();
        let expires_at = Instant::now();
        let old = DecisionRequest::seal(parts.clone(), &key, expires_at, &RequestLimits::default()).unwrap();
        let default = DecisionRequest::seal_with_projection_audit(
            parts.clone(),
            ProjectionAudit::default(),
            &key,
            expires_at,
            &RequestLimits::default(),
        )
        .unwrap();
        assert_eq!(
            old.manifest().request_fingerprint(),
            default.manifest().request_fingerprint()
        );
        let audit = ProjectionAudit {
            truncated_fields: vec!["artifacts.summary".into()],
            redacted_texts: 1,
        };
        let projected = DecisionRequest::seal_with_projection_audit(
            parts,
            audit.clone(),
            &key,
            expires_at,
            &RequestLimits::default(),
        )
        .unwrap();
        assert_ne!(
            old.manifest().request_fingerprint(),
            projected.manifest().request_fingerprint()
        );
        assert_eq!(projected.manifest().projection_audit(), &audit);
    }

    #[test]
    fn typed_ids_follow_their_grammar() {
        for question in GlobalQuestion::ALL {
            assert!(QuestionId::global(question).is_well_formed(), "{question:?}");
        }
        let longest = CandidateKey::from_parts("request_approval", "native_tool", Some([0xab, 0xcd, 0x12, 0x34]));
        assert_eq!(longest.as_str(), "request_approval-native_tool-abcd1234");
        for question in CandidateQuestion::ALL {
            let id = QuestionId::candidate(&longest, question);
            assert!(id.is_well_formed(), "{}", id.as_str());
            assert!(
                id.as_str()
                    .starts_with("candidate.request_approval-native_tool-abcd1234.")
            );
        }
        assert_eq!(OptionId::candidate(&longest).as_str(), longest.as_str());
        assert!(
            !QuestionId::candidate(
                &CandidateKey::from_parts("Use Tool", "mcp", None),
                CandidateQuestion::Gain
            )
            .is_well_formed()
        );
        assert!(!is_code_segment(&"a".repeat(48)));
        assert!(is_code_segment(&"a".repeat(47)));
    }

    #[test]
    fn structured_limits_are_enforced_iteratively() {
        let mut deep = serde_json::json!("leaf");
        for _ in 0..64 {
            deep = serde_json::json!([deep]);
        }
        assert_eq!(
            StructuredValue::try_from(deep),
            Err(DecisionError::InvalidRequest(RequestRule::StructuredDepth))
        );
        let mut exactly = serde_json::json!("leaf");
        for _ in 0..15 {
            exactly = serde_json::json!([exactly]);
        }
        assert!(StructuredValue::try_from(exactly).is_ok());
        let wide = serde_json::Value::Array(vec![serde_json::json!(1); 4096]);
        assert_eq!(
            StructuredValue::try_from(wide),
            Err(DecisionError::InvalidRequest(RequestRule::StructuredNodes))
        );
        let key_too_long = serde_json::json!({ "k".repeat(8 * 1024 + 1): 1 });
        assert_eq!(
            StructuredValue::try_from(key_too_long),
            Err(DecisionError::InvalidRequest(RequestRule::StructuredStringLength))
        );
        assert_eq!(
            StructuredValue::try_from(serde_json::json!({"a": "bell\u{07}"})),
            Err(DecisionError::InvalidRequest(RequestRule::StructuredText))
        );
        assert!(StructuredValue::try_from(serde_json::json!({"a": "tab\tnew\nline\r"})).is_ok());
        let stricter = StructuredLimits {
            max_depth: 1,
            ..StructuredLimits::DEFAULT
        };
        assert_eq!(
            text("x").check(&stricter),
            Ok(()),
            "a string is depth 1 and fits a depth-1 limit"
        );
    }

    #[test]
    fn no_depth_limit_disables_the_depth_check() {
        let mut deep = serde_json::json!("leaf");
        for _ in 0..299 {
            deep = serde_json::json!([deep]);
        }
        let widest = StructuredLimits {
            max_depth: u8::MAX,
            max_nodes: u32::MAX,
            max_string_bytes: 8 * 1024,
        };
        assert_eq!(
            StructuredValue::with_limits(deep, &widest),
            Err(DecisionError::InvalidRequest(RequestRule::StructuredDepth))
        );
        let mut at_limit = serde_json::json!("leaf");
        for _ in 0..254 {
            at_limit = serde_json::json!([at_limit]);
        }
        assert!(StructuredValue::with_limits(at_limit, &widest).is_ok());
    }

    #[test]
    fn identity_text_refuses_controls_and_bidi_marks() {
        for bad in [
            "",
            "   ",
            "jev\u{1b}[2J",
            "jev\nnext",
            "jev\u{85}",
            "jev\u{202e}gnp",
            "jev\u{2066}x",
            "jev\u{200f}",
            "jev\u{2028}x",
            "jev\u{2029}x",
            "jev\u{200b}-1.13.0",
            "\u{feff}jev",
            "jev\u{2060}",
        ] {
            assert!(!is_identity_text(bad), "{bad:?}");
        }
        assert!(is_identity_text("typesafe/jev-1.13-20260917"));
        assert!(is_identity_text(&"m".repeat(128)));
        assert!(!is_identity_text(&"m".repeat(129)));
    }

    #[test]
    fn structured_values_never_print_their_content() {
        let value =
            StructuredValue::try_from(serde_json::json!({"latest_user_turn": "my secret plan"})).expect("test: object");
        assert_eq!(format!("{value:?}"), "StructuredValue(<object>)");
        assert_eq!(format!("{:?}", text("my secret plan")), "StructuredValue(<string>)");
    }

    #[test]
    fn canonical_json_matches_serde_json_on_a_sorted_value() {
        let value = serde_json::json!({
            "z": [1, 2.5, -3, true, null],
            "a": {"y": "quote\" back\\ ctl\u{01}\u{1f} nl\n", "b": "\u{08}\u{0C}\t\r/"},
            "m": "non-ascii \u{00e9}\u{0416}"
        });
        let mut canonical = String::new();
        write_canonical(&value, &mut canonical);
        let sorted: serde_json::Value = serde_json::from_str(&canonical).expect("test: reparse");
        assert_eq!(sorted, value);
        let reference = serde_json::to_string(&sorted).expect("test: serialize");
        assert_eq!(canonical, reference);
    }

    #[test]
    fn probabilities_refuse_every_non_probability() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0001, 1.0001, -1.0, 2.0] {
            assert_eq!(
                Probability::new(value),
                Err(DecisionError::InvalidResponse(ValidationRule::ProbabilityRange)),
                "{value}"
            );
        }
        for value in [0.0, 1.0, 0.5, f64::MIN_POSITIVE, 1.0 - f64::EPSILON] {
            assert_eq!(
                Probability::new(value).map(Probability::get).map(f64::to_bits),
                Ok(value.to_bits())
            );
        }
    }

    #[test]
    fn a_question_is_written_in_its_wire_shape() {
        let mut out = String::new();
        write_question(
            &DecisionQuestion::Noul {
                instructions: text("Is it?"),
                criteria: Some(NoulCriteria {
                    when_true: Some(text("yes")),
                    when_false: None,
                }),
            },
            &mut out,
        );
        assert_eq!(
            out,
            r#"{"criteria":{"true":"yes"},"instructions":"Is it?","type":"noul"}"#
        );
        out.clear();
        write_question(
            &DecisionQuestion::Score {
                instructions: text("How much?"),
                levels: vec![text("low"), text("high")],
            },
            &mut out,
        );
        assert_eq!(
            out,
            r#"{"criteria":["low","high"],"instructions":"How much?","type":"score"}"#
        );
    }
}

#[cfg(test)]
pub(crate) mod ownership;

#[cfg(test)]
mod structured_validation;
