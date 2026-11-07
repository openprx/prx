//! Backend contracts for atomic durable ingress reception and interruption evidence.
//! A receipt or recovered row never grants permission to execute its input again.
use super::{MessageEvent, MessageEventInput};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressRecoveryScope {
    pub workspace_id: String,
    pub owner_id: String,
    pub source: super::MessageEventSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressCapsule {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    pub backend_namespace: String,
    pub scope: IngressRecoveryScope,
    pub lifetime: String,
    pub input_event_id: String,
    pub input_key: String,
    pub run_id: String,
    /// Digest of the complete normalized input, including its original payload.
    pub input_digest: String,
    pub authentication: String,
}

/// Held by cleanup ownership, never merely by a request's stack frame.
/// PostgreSQL validity means database ownership, not remote provider liveness.
#[async_trait::async_trait]
pub trait IngressLifetime: Send + Sync {
    fn token(&self) -> &str;
    fn namespace(&self) -> &str;
    fn loss_signal(&self) -> Option<tokio_util::sync::CancellationToken> {
        None
    }
    async fn validate(&self, deadline: std::time::Instant) -> anyhow::Result<()>;
}

pub enum IngressAppendOutcome {
    Inserted(MessageEvent),
    /// Immutable prior receipt. The runtime must reconcile/report it, not execute.
    AlreadyAccepted(MessageEvent),
    /// The stable input key was accepted before and its record was erased. Nothing
    /// was inserted, and no content-derived digest of the erased input survives.
    Tombstoned,
}

/// Pending-attestation inputs the repository publishes in the append transaction.
/// The tags are repository-keyed; `facts` is the closed [`PendingFacts`] JSON.
/// `verified` is set only by [`verify_transport`], at the first layer that holds
/// the plaintext accepted input.
#[derive(Debug)]
pub struct IngressAttestation {
    pub facts: Vec<u8>,
    pub transport: super::recovery_authority::RecoveryTag,
    pub input_key: super::recovery_authority::RecoveryTag,
    verified: Option<VerifiedTransport>,
}

/// Proof that the transport tag was verified against the accepted input. Only
/// [`verify_transport`] can build it: private fields, no `Default`, no `Deserialize`,
/// not `Clone`.
#[derive(Debug)]
struct VerifiedTransport {
    input_event_id: String,
}

impl IngressAttestation {
    #[must_use]
    pub const fn new(
        facts: Vec<u8>,
        transport: super::recovery_authority::RecoveryTag,
        input_key: super::recovery_authority::RecoveryTag,
    ) -> Self {
        Self {
            facts,
            transport,
            input_key,
            verified: None,
        }
    }

    /// The input event the transport tag was verified for, if it was verified.
    pub(crate) fn verified_input_event_id(&self) -> Option<&str> {
        self.verified.as_ref().map(|verified| verified.input_event_id.as_str())
    }

    /// Whether the first trusted layer already verified this attestation.
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        self.verified.is_some()
    }
}

impl Clone for IngressAttestation {
    /// A clone never inherits the verification token.
    fn clone(&self) -> Self {
        Self {
            facts: self.facts.clone(),
            transport: self.transport.clone(),
            input_key: self.input_key.clone(),
            verified: None,
        }
    }
}

/// The transport preimage: the stable transport content of an input, with its
/// payload replaced by the pre-envelope `original_payload`. Local retry run and
/// config generation may change; creator/lease and transport attribution never do.
#[must_use]
pub fn transport_preimage(input: &MessageEventInput, original_payload: Option<&str>) -> Vec<u8> {
    let view = (
        &input.workspace_id,
        &input.owner_id,
        &input.source,
        &input.channel,
        &input.session_key,
        (
            &input.parent_session_key,
            &input.parent_run_id,
            &input.agent_id,
            &input.persona_id,
            &input.sender,
            &input.recipient,
        ),
        (
            &input.role,
            &input.event_type,
            &input.subject,
            &input.goal_id,
            &input.causation_event_id,
            &input.correlation_id,
        ),
        (
            &input.attempt_id,
            &input.lease_epoch,
            &input.content,
            original_payload,
            &input.visibility,
            &input.idempotency_key,
        ),
    );
    // Serializing borrowed strings and options cannot fail.
    serde_json::to_vec(&view).unwrap_or_default()
}

/// Verify the transport tag against the accepted plaintext input, and bind the
/// result to the input event. Run it once, at the first layer that holds the
/// plaintext (before any sealing). A MAC failure is a mismatch; an unusable key
/// propagates unchanged.
pub fn verify_transport(
    repo: &dyn super::recovery_authority::RecoveryRepository,
    input: &MessageEventInput,
    capsule: &IngressCapsule,
    attestation: &mut IngressAttestation,
) -> anyhow::Result<()> {
    let mismatch = || anyhow::Error::from(IngressAttestationError::Mismatch("transport_digest"));
    let raw = input.raw_payload_json.as_deref().ok_or_else(mismatch)?;
    anyhow::ensure!(raw.len() <= MAX_RECOVERY_EVENT_BYTES, "oversized ingress payload");
    let envelope: serde_json::Value = serde_json::from_str(raw).map_err(|_| mismatch())?;
    let stored = envelope.get("prx_ingress_capsule").ok_or_else(mismatch)?;
    let decoded = IngressCapsule::decode(&serde_json::to_string(stored)?).map_err(|_| mismatch())?;
    if serde_json::to_vec(&decoded)? != serde_json::to_vec(capsule)? {
        return Err(mismatch());
    }
    let original = match envelope.get("original_payload_json") {
        Some(serde_json::Value::String(text)) => Some(text.as_str()),
        Some(serde_json::Value::Null) | None => None,
        Some(_) => return Err(mismatch()),
    };
    if input.event_id.as_deref() != Some(capsule.input_event_id.as_str()) {
        return Err(mismatch());
    }
    let facts = PendingFacts::parse(&attestation.facts)?;
    facts.check_capsule(capsule)?;
    let tag_matches = facts.key_id == attestation.transport.key_id
        && same_bytes(
            facts.transport_digest.as_bytes(),
            hex::encode(attestation.transport.tag).as_bytes(),
        );
    if !tag_matches {
        return Err(mismatch());
    }
    match repo.verify_recovery_mac(
        &attestation.transport,
        super::recovery_authority::RecoveryMacDomain::TransportV3,
        &transport_preimage(input, original),
    ) {
        Ok(()) => {}
        Err(error) if super::recovery_authority::is_key_unavailable(&error) => return Err(error),
        Err(_) => return Err(mismatch()),
    }
    attestation.verified = Some(VerifiedTransport {
        input_event_id: capsule.input_event_id.clone(),
    });
    Ok(())
}

/// The stable input key was accepted before and its record was erased; this
/// input is refused without a row.
#[derive(Debug, thiserror::Error)]
#[error("stable input key was accepted before and its record was erased")]
pub struct StableKeyTombstoned;

/// The tombstone fence could not load a key a tombstone in this scope needs.
/// It carries no key id, digest or load error, and is deliberately not a
/// `RecoveryKeyUnavailable`: no path may turn it into "unavailable, continue".
#[derive(Debug)]
pub struct StableKeyFenceUnavailable {
    pub workspace: String,
    pub source: String,
}

impl std::fmt::Display for StableKeyFenceUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("stable input key fence unavailable: a recovery key needed by this scope is missing")
    }
}

impl std::error::Error for StableKeyFenceUnavailable {}

/// Why an attestation was refused. Each variant is a distinct, typed refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IngressAttestationError {
    #[error("this backend cannot publish an ingress attestation")]
    Unsupported,
    #[error("ingress attestation facts carry an unknown key")]
    UnknownKey,
    #[error("ingress attestation facts are malformed")]
    Malformed,
    #[error("ingress attestation fact `{0}` does not match the input")]
    Mismatch(&'static str),
    #[error("an accepted v3 ingress input has no attestation")]
    Missing,
}

/// Refuse an attestation on a backend that has no recovery repository. The
/// attestation is never silently dropped.
pub fn refuse_attestation(attestation: Option<&IngressAttestation>) -> anyhow::Result<()> {
    match attestation {
        Some(_) => Err(IngressAttestationError::Unsupported.into()),
        None => Ok(()),
    }
}

/// Constant-time equality of two byte strings, via `ring::hmac`.
pub(crate) fn same_bytes(left: &[u8], right: &[u8]) -> bool {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[0; 32]);
    ring::hmac::verify(&key, left, ring::hmac::sign(&key, right).as_ref()).is_ok()
}

const FACT_KEYS: [&str; 18] = [
    "v",
    "namespace",
    "workspace_id",
    "owner_id",
    "source",
    "channel",
    "session_key",
    "surface",
    "generation",
    "lifetime",
    "input_event_id",
    "input_key",
    "capsule_version",
    "capsule",
    "input_digest",
    "transport_digest",
    "input_key_digest",
    "key_id",
];

/// Closed v1 schema of the pending attestation facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingFacts {
    pub v: u32,
    pub namespace: String,
    pub workspace_id: String,
    pub owner_id: String,
    pub source: String,
    pub channel: Option<String>,
    pub session_key: Option<String>,
    pub surface: String,
    /// The accepted input's config_generation_id, with None normalized to the sentinel 0. Bound for audit only: 0 does not distinguish "no generation" from generation 0, so consumers must not infer from this field whether the input carried a generation. The transport tag excludes the generation, so a retry under a new generation is not a conflict.
    pub generation: u64,
    pub lifetime: String,
    pub input_event_id: String,
    pub input_key: String,
    pub capsule_version: u32,
    /// Base64 of the exact serialized capsule bytes.
    pub capsule: String,
    pub input_digest: String,
    pub transport_digest: String,
    pub input_key_digest: String,
    pub key_id: String,
}

impl PendingFacts {
    /// Parse the closed schema: an unknown or missing key refuses.
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| IngressAttestationError::Malformed)?;
        let object = value.as_object().ok_or(IngressAttestationError::Malformed)?;
        if object.keys().any(|key| !FACT_KEYS.contains(&key.as_str())) {
            return Err(IngressAttestationError::UnknownKey.into());
        }
        if FACT_KEYS.iter().any(|key| !object.contains_key(*key)) {
            return Err(IngressAttestationError::Malformed.into());
        }
        let facts: Self = serde_json::from_value(value).map_err(|_| IngressAttestationError::Malformed)?;
        if facts.v != 1 {
            return Err(IngressAttestationError::Mismatch("v").into());
        }
        Ok(facts)
    }

    /// Check every fact that the capsule alone determines.
    pub fn check_capsule(&self, capsule: &IngressCapsule) -> anyhow::Result<()> {
        use base64::Engine as _;
        fn same(ok: bool, name: &'static str) -> anyhow::Result<()> {
            if ok {
                Ok(())
            } else {
                Err(IngressAttestationError::Mismatch(name).into())
            }
        }
        same(self.namespace == capsule.backend_namespace, "namespace")?;
        same(self.workspace_id == capsule.scope.workspace_id, "workspace_id")?;
        same(self.owner_id == capsule.scope.owner_id, "owner_id")?;
        same(self.source == capsule.scope.source.as_str(), "source")?;
        same(self.lifetime == capsule.lifetime, "lifetime")?;
        same(self.input_event_id == capsule.input_event_id, "input_event_id")?;
        same(self.input_key == capsule.input_key, "input_key")?;
        same(
            self.capsule_version == capsule.version && capsule.version == 3,
            "capsule_version",
        )?;
        let exact = serde_json::to_vec(capsule)?;
        same(
            base64::engine::general_purpose::STANDARD
                .decode(&self.capsule)
                .is_ok_and(|bytes| bytes == exact),
            "capsule",
        )?;
        same(
            same_bytes(self.input_digest.as_bytes(), capsule.input_digest.as_bytes()),
            "input_digest",
        )?;
        same(capsule.key_id.as_deref() == Some(self.key_id.as_str()), "key_id")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressRecoveryCandidate {
    pub cursor: i64,
    pub capsule: IngressCapsule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngressRecoveryState {
    Pending,
    Unconfirmed,
    Reconciled,
}

/// Acquisition proves the backend lease is available, never that effects ended.
/// Keep this object alive through the evidence transaction.
pub struct IngressRecoveryClaim {
    pub lifetime: Arc<dyn IngressLifetime>,
}

pub struct IngressRecoveryCommit {
    pub capsule: IngressCapsule,
    pub state: IngressRecoveryState,
    pub evidence: MessageEventInput,
    pub claim: IngressRecoveryClaim,
    pub deadline: std::time::Instant,
}

/// Internal transaction hook, shared with the existing message/outbox append.
#[expect(
    clippy::large_enum_variant,
    reason = "Inline atomic ownership preserves the recovery transaction allocation profile"
)]
pub(crate) enum AtomicIngressMutation {
    Input {
        capsule: IngressCapsule,
        lifetime: Arc<dyn IngressLifetime>,
        deadline: std::time::Instant,
        attestation: Option<IngressAttestation>,
    },
    Recovery(IngressRecoveryCommit),
}
impl AtomicIngressMutation {
    pub(crate) const fn capsule(&self) -> &IngressCapsule {
        match self {
            Self::Input { capsule, .. } => capsule,
            Self::Recovery(commit) => &commit.capsule,
        }
    }
}

pub const MAX_RECOVERY_PAGE: usize = 64;
pub const MAX_CAPSULE_BYTES: usize = 16 * 1024;
pub const MAX_ID_BYTES: usize = 4096;

impl IngressCapsule {
    pub fn validate_bounds(&self) -> anyhow::Result<()> {
        match (self.version, self.key_id.as_deref()) {
            (1 | 2, None) => {}
            (3, Some(id)) => anyhow::ensure!(
                uuid::Uuid::parse_str(id)?.to_string() == id,
                "non-canonical recovery key id"
            ),
            _ => anyhow::bail!("unsupported ingress capsule version or key id"),
        }
        for value in [
            &self.scope.workspace_id,
            &self.scope.owner_id,
            &self.input_event_id,
            &self.input_key,
            &self.run_id,
        ] {
            anyhow::ensure!(
                !value.is_empty() && value.len() <= MAX_ID_BYTES,
                "invalid ingress identifier size"
            );
        }
        anyhow::ensure!(
            uuid::Uuid::parse_str(&self.backend_namespace).is_ok(),
            "invalid ingress backend namespace"
        );
        anyhow::ensure!(
            uuid::Uuid::parse_str(&self.lifetime).is_ok(),
            "invalid ingress lifetime"
        );
        anyhow::ensure!(
            self.input_digest.len() == 64 && self.authentication.len() == 64,
            "invalid ingress authentication size"
        );
        anyhow::ensure!(
            !matches!(self.scope.source, super::MessageEventSource::Other(_)),
            "unsupported ingress source"
        );
        Ok(())
    }
    pub fn decode(encoded: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(encoded.len() <= MAX_CAPSULE_BYTES, "ingress capsule exceeds byte limit");
        let value: Self = serde_json::from_str(encoded)?;
        value.validate_bounds()?;
        Ok(value)
    }
}

pub(crate) fn remaining(deadline: std::time::Instant) -> anyhow::Result<std::time::Duration> {
    deadline
        .checked_duration_since(crate::causal_tree::logical_clock::deadline_now(deadline))
        .filter(|left| !left.is_zero())
        .ok_or_else(|| anyhow::anyhow!("ingress recovery deadline elapsed"))
}

#[derive(Debug, Clone)]
pub enum IngressEventLookup {
    /// Exact key within workspace, owner, and source.
    EventId(String),
    IdempotencyKey(String),
    /// Exact key within workspace and owner; callers authenticate source lineage.
    OwnedEventId(String),
    OwnedIdempotencyKey(String),
}
pub const MAX_RECOVERY_EVENT_BYTES: usize = 1024 * 1024;

/// One bounded lifetime cache belongs to the concrete backend, not to a turn.
/// Failed sessions remain failed until that backend owner is replaced.
#[derive(Default)]
pub(crate) struct IngressOwnerCache {
    owners: parking_lot::Mutex<std::collections::BTreeMap<String, Arc<IngressOwnerEntry>>>,
}
impl IngressOwnerCache {
    pub(crate) fn cell(&self, workspace: &str) -> anyhow::Result<Arc<IngressOwnerEntry>> {
        anyhow::ensure!(
            !workspace.is_empty() && workspace.len() <= MAX_ID_BYTES,
            "invalid ingress owner workspace"
        );
        let mut owners = self.owners.lock();
        if let Some(cell) = owners.get(workspace) {
            return Ok(cell.clone());
        }
        anyhow::ensure!(owners.len() < 16, "backend ingress workspace capacity exhausted");
        let cell = Arc::new(IngressOwnerEntry::default());
        owners.insert(workspace.to_owned(), cell.clone());
        Ok(cell)
    }
}

/// Count serialized bytes before allocating a payload buffer or cloning input.
pub(crate) fn serialized_within<T: serde::Serialize>(value: &T, limit: usize) -> anyhow::Result<()> {
    struct Counter {
        remaining: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.remaining = self
                .remaining
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("ingress serialization exceeds byte budget"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter { remaining: limit }, value)?;
    Ok(())
}

#[derive(Default)]
pub(crate) struct IngressOwnerEntry {
    pub(crate) cell: tokio::sync::OnceCell<Arc<dyn IngressLifetime>>,
    pub(crate) init_gate: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone, Copy)]
pub enum IngressRecoveryScan {
    PendingOnly,
    IncludeUnconfirmed,
}

pub(crate) fn input_within_bounds(input: &MessageEventInput, limit: usize) -> anyhow::Result<()> {
    let mut total = 0usize;
    let mut add = |value: &str, maximum: usize| -> anyhow::Result<()> {
        anyhow::ensure!(value.len() <= maximum, "ingress field exceeds byte budget");
        total = total
            .checked_add(value.len())
            .filter(|total| *total <= limit)
            .ok_or_else(|| anyhow::anyhow!("ingress input exceeds byte budget"))?;
        Ok(())
    };
    add(&input.workspace_id, MAX_ID_BYTES)?;
    add(input.source.as_str(), MAX_ID_BYTES)?;
    add(&input.role, MAX_ID_BYTES)?;
    add(&input.event_type, MAX_ID_BYTES)?;
    for value in [
        &input.event_id,
        &input.idempotency_key,
        &input.owner_id,
        &input.channel,
        &input.session_key,
        &input.parent_session_key,
        &input.run_id,
        &input.parent_run_id,
        &input.agent_id,
        &input.persona_id,
        &input.sender,
        &input.recipient,
        &input.goal_id,
        &input.causation_event_id,
        &input.correlation_id,
        &input.attempt_id,
        &input.config_source_revision,
    ]
    .into_iter()
    .flatten()
    {
        add(value, MAX_ID_BYTES)?;
    }
    if let Some(subject) = &input.subject {
        use super::MessageEventSubject::{
            Conversation, Customer, Document, Goal, Incident, Other, Project, Task, Topic,
        };
        match subject {
            Project(value) | Task(value) | Incident(value) | Customer(value) | Document(value) | Goal(value)
            | Topic(value) | Conversation(value) => add(value, MAX_ID_BYTES)?,
            Other { subject_type, id } => {
                add(subject_type, MAX_ID_BYTES)?;
                add(id, MAX_ID_BYTES)?;
            }
        }
    }
    add(&input.content, limit)?;
    if let Some(raw) = &input.raw_payload_json {
        add(raw, limit)?;
    }
    serialized_within(input, limit)
}
