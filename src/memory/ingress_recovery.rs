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
        anyhow::ensure!(self.version == 1, "unsupported ingress capsule version");
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
        .checked_duration_since(std::time::Instant::now())
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
