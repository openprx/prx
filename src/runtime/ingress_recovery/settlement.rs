//! Per-row recovery outcomes, the authenticated settlement evidence they
//! write, and the owner's own settlement of the input it recorded.
//!
//! No prior row can stop new input: each ends a recovery pass in exactly one
//! [`RecoveryRowOutcome`], recorded in the existing evidence rows and logged
//! once, without payload.
use super::authority::capsule_mac;
use crate::memory::ingress_recovery::{
    IngressAttestationError, IngressCapsule, IngressLifetime, IngressRecoveryClaim, IngressRecoveryCommit,
    IngressRecoveryState, OrphanCause,
};
use crate::memory::recovery_authority::{RecoveryChain, RecoveryMacDomain, RecoveryRepository};
use crate::memory::{Memory, MemoryVisibility, MessageEventInput};
use anyhow::Context;
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Weak};
use std::time::Instant;

/// The one outcome a prior row ends a recovery pass with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryRowOutcome {
    /// Its lifetime ended and its outcome is durably recorded: reconciled with
    /// an authenticated final, or interrupted without one.
    Settled,
    /// A live lifetime may still own the row; it is left untouched.
    SkippedLive,
    /// Recorded `unconfirmed`, with its cause bound into the evidence.
    Orphaned(OrphanCause),
    /// Left as it is, with an authenticated quarantine marker; retried at most
    /// once per process start.
    Quarantined(QuarantineCause),
    /// Already recorded `unconfirmed` with its evidence and still without a
    /// final: nothing was written, so nothing is reported.
    Unchanged,
}

impl RecoveryRowOutcome {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::SkippedLive => "skipped_live",
            Self::Orphaned(_) => "orphaned",
            Self::Quarantined(_) => "quarantined",
            Self::Unchanged => "unchanged",
        }
    }

    const fn cause(self) -> &'static str {
        match self {
            Self::Settled | Self::SkippedLive | Self::Unchanged => "none",
            Self::Orphaned(cause) => cause.as_str(),
            Self::Quarantined(cause) => cause.as_str(),
        }
    }
}

/// Why a prior row is quarantined. Only the code is persisted, never error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuarantineCause {
    /// The row's lock exists but cannot be used safely.
    LockUnusable,
    /// The row's input could not be read.
    InputUnreadable,
    /// The capsule's authentication, version or backend does not verify.
    CapsuleAuthentication,
    /// The input differs from the digest its capsule authenticates.
    InputDigest,
    /// The input does not verify against its capsule for another reason.
    InputUnverified,
    /// The run's final could not be read.
    TerminalUnreadable,
    /// The run's final does not authenticate.
    TerminalAuthentication,
    /// The settlement evidence was refused or did not read back.
    EvidenceRejected,
}

impl QuarantineCause {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::LockUnusable => "lock_unusable",
            Self::InputUnreadable => "input_unreadable",
            Self::CapsuleAuthentication => "capsule_authentication",
            Self::InputDigest => "input_digest",
            Self::InputUnverified => "input_unverified",
            Self::TerminalUnreadable => "terminal_unreadable",
            Self::TerminalAuthentication => "terminal_authentication",
            Self::EvidenceRejected => "evidence_rejected",
        }
    }

    /// Classify a failed verification of a row's input against its capsule.
    pub(super) fn of_input(error: &anyhow::Error) -> Self {
        for cause in error.chain() {
            if matches!(
                cause.downcast_ref::<IngressAttestationError>(),
                Some(IngressAttestationError::Mismatch("capsule_version"))
            ) {
                return Self::CapsuleAuthentication;
            }
            match cause.to_string().as_str() {
                "invalid ingress capsule authentication" | "ingress capsule belongs to another backend" => {
                    return Self::CapsuleAuthentication;
                }
                "ingress input digest differs from its capsule" => return Self::InputDigest,
                _ => {}
            }
        }
        Self::InputUnverified
    }

    /// Classify a failed rejoin of a row's final. A terminal whose restart
    /// verification fails (D§4.4), including one without a settled receipt,
    /// is never re-executed or settled.
    pub(super) fn of_terminal(error: &anyhow::Error) -> Self {
        const UNAUTHENTICATED: &[&str] = &[
            "terminal replay authentication failed",
            "terminal replay event authentication failed",
            "invalid ingress capsule authentication",
            "unsigned terminal cannot be replayed",
            "invalid terminal replay signature",
            "terminal replay binding does not match the committed marker",
            "original terminal source event is missing",
            "terminal replay decision row is missing",
        ];
        let unauthenticated = error.chain().any(|cause| {
            cause.is::<crate::runtime::turn_terminal::restart_verify::TerminalNotReplayable>()
                || matches!(
                    cause.downcast_ref::<IngressAttestationError>(),
                    Some(IngressAttestationError::Mismatch("capsule_version"))
                )
                || UNAUTHENTICATED.contains(&cause.to_string().as_str())
        });
        if unauthenticated {
            Self::TerminalAuthentication
        } else {
            Self::TerminalUnreadable
        }
    }
}

/// A row-scoped failure and the quarantine cause it maps to.
pub(super) struct RowFailure {
    pub(super) cause: QuarantineCause,
    pub(super) error: anyhow::Error,
}

pub(super) trait RowStep<T> {
    /// Tag a failed recovery step with its quarantine cause.
    fn at(self, cause: QuarantineCause) -> Result<T, RowFailure>;
}

impl<T> RowStep<T> for anyhow::Result<T> {
    fn at(self, cause: QuarantineCause) -> Result<T, RowFailure> {
        self.map_err(|error| RowFailure { cause, error })
    }
}

/// Who writes a settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Settler {
    /// A later lifetime, after the owner ended or its lock went missing.
    Successor,
    /// The owner, in the transaction that commits the run's final.
    Owner,
}

/// The authenticated evidence row of one settlement, built from `evidence`
/// (the verified original input, or [`capsule_evidence_base`] when the input
/// is missing). Its MAC binds the state, the capsule and the orphan cause.
pub(super) fn settlement_evidence(
    repository: &dyn RecoveryRepository,
    mut evidence: MessageEventInput,
    capsule: &IngressCapsule,
    state: IngressRecoveryState,
    cause: Option<OrphanCause>,
    settler: Settler,
) -> anyhow::Result<MessageEventInput> {
    let label = match state {
        IngressRecoveryState::Pending => anyhow::bail!("recovery cannot restore pending authority"),
        IngressRecoveryState::Unconfirmed => "unconfirmed",
        IngressRecoveryState::Reconciled => "reconciled",
    };
    // Absence of a verified final is never interpreted as no charge or no
    // provider execution. A late final remains replayable by the existing
    // signed terminal verifier.
    evidence.event_id = None;
    evidence.causation_event_id = Some(capsule.input_event_id.clone());
    evidence.idempotency_key = Some(format!("ingress:{}:{label}", capsule.input_event_id));
    "event".clone_into(&mut evidence.role);
    let (event_type, content) = match (state, settler) {
        (IngressRecoveryState::Reconciled, Settler::Owner) => (
            "turn.ingress.reconciled",
            "turn.ingress.reconciled final committed by its owner",
        ),
        (IngressRecoveryState::Reconciled, Settler::Successor) => (
            "turn.ingress.reconciled",
            "turn.ingress.reconciled authenticated final evidence rejoined",
        ),
        _ => (
            "turn.ingress.interrupted",
            "turn.ingress.interrupted outcome and usage remain unconfirmed",
        ),
    };
    event_type.clone_into(&mut evidence.event_type);
    evidence.content = cause.map_or_else(
        || content.to_owned(),
        |cause| format!("{content} cause={}", cause.as_str()),
    );
    // The MAC binds the state, the capsule and, for an orphan, its cause.
    let state_bytes = cause.map_or_else(
        || serde_json::to_vec(&(label, capsule)),
        |cause| serde_json::to_vec(&(label, capsule, cause.as_str())),
    )?;
    let authentication = hex::encode(capsule_mac(
        repository,
        capsule,
        RecoveryMacDomain::StateV3,
        &state_bytes,
    )?);
    let mut binding = serde_json::json!({
        "capsule": capsule,
        "state": label,
        "evidence_authentication": authentication,
    });
    if let Some(cause) = cause {
        binding
            .as_object_mut()
            .context("ingress evidence binding is not an object")?
            .insert("cause".to_owned(), cause.as_str().into());
    }
    evidence.raw_payload_json = Some(serde_json::to_string(
        &serde_json::json!({ "ingress_recovery": binding }),
    )?);
    Ok(evidence)
}

/// Evidence fields of a row whose input is missing: only what the capsule
/// itself authenticates. It carries no content.
pub(super) fn capsule_evidence_base(capsule: &IngressCapsule) -> MessageEventInput {
    MessageEventInput {
        agent_partition: None,
        event_id: None,
        idempotency_key: None,
        workspace_id: capsule.scope.workspace_id.clone(),
        owner_id: Some(capsule.scope.owner_id.clone()),
        source: capsule.scope.source.clone(),
        channel: None,
        session_key: None,
        parent_session_key: None,
        run_id: Some(capsule.run_id.clone()),
        parent_run_id: None,
        agent_id: None,
        persona_id: None,
        sender: None,
        recipient: None,
        role: String::new(),
        event_type: String::new(),
        subject: None,
        goal_id: None,
        causation_event_id: None,
        correlation_id: None,
        attempt_id: None,
        lease_epoch: None,
        config_generation_id: None,
        config_source_revision: None,
        content: String::new(),
        raw_payload_json: None,
        visibility: MemoryVisibility::System,
    }
}

/// An input this process recorded and still owns, until its terminal settles it.
struct OwnedInput {
    chain: RecoveryChain,
    capsule: IngressCapsule,
    /// The verified input without content or payload; settlement replaces both.
    evidence: MessageEventInput,
    lifetime: Weak<dyn IngressLifetime>,
}

/// Inputs awaiting their terminal. When full, the oldest is left to recovery.
const MAX_OWNED_INPUTS: usize = 128;

fn owned_inputs() -> &'static parking_lot::Mutex<VecDeque<OwnedInput>> {
    static OWNED: std::sync::OnceLock<parking_lot::Mutex<VecDeque<OwnedInput>>> = std::sync::OnceLock::new();
    OWNED.get_or_init(Default::default)
}

/// Remember an input this lifetime recorded, so its terminal commit settles it.
pub(super) fn register_owned_input(
    capsule: &IngressCapsule,
    mut verified: MessageEventInput,
    lifetime: &Arc<dyn IngressLifetime>,
) {
    verified.content.clear();
    verified.raw_payload_json = None;
    let chain = RecoveryChain {
        workspace_id: capsule.scope.workspace_id.clone(),
        owner_id: capsule.scope.owner_id.clone(),
        source: capsule.scope.source.as_str().to_owned(),
        run_id: capsule.run_id.clone(),
    };
    let mut owned = owned_inputs().lock();
    owned.retain(|entry| entry.lifetime.strong_count() > 0 && entry.chain != chain);
    if owned.len() >= MAX_OWNED_INPUTS {
        owned.pop_front();
        tracing::warn!(
            capacity = MAX_OWNED_INPUTS,
            "ingress owner settlement capacity reached; the oldest unsettled input is left to recovery"
        );
    }
    owned.push_back(OwnedInput {
        chain,
        capsule: capsule.clone(),
        evidence: verified,
        lifetime: Arc::downgrade(lifetime),
    });
}

/// The owner's settlement of the input it recorded for `chain`'s run: the
/// row is reconciled in the same transaction as the run's final, under the
/// owner's own live lifetime. `None` when this process recorded no such input
/// or no longer holds a valid lifetime; the row is then left to recovery.
pub(crate) async fn owner_settlement(
    memory: &Arc<dyn Memory>,
    chain: &RecoveryChain,
    deadline: Instant,
) -> Option<IngressRecoveryCommit> {
    let (capsule, evidence, lifetime) = {
        let owned = owned_inputs().lock();
        let entry = owned.iter().find(|entry| entry.chain == *chain)?;
        (entry.capsule.clone(), entry.evidence.clone(), entry.lifetime.upgrade()?)
    };
    // A settlement the owner gives up is never offered again: a replay of the
    // same terminal must commit exactly the events its first commit did.
    if let Err(error) = lifetime.validate(deadline).await {
        owner_settled(chain);
        tracing::warn!(
            error = %crate::agent::diagnostics::error_class(&error),
            "ingress owner lifetime failed validation; its input is left to recovery"
        );
        return None;
    }
    let memory = Arc::clone(memory);
    let built = crate::runtime::blocking::spawn_blocking(move || {
        let repository = memory
            .recovery_repository()
            .context("recovery repository unavailable")?;
        let evidence = settlement_evidence(
            repository,
            evidence,
            &capsule,
            IngressRecoveryState::Reconciled,
            None,
            Settler::Owner,
        )?;
        Ok::<_, anyhow::Error>(IngressRecoveryCommit {
            capsule,
            state: IngressRecoveryState::Reconciled,
            evidence,
            claim: IngressRecoveryClaim { lifetime },
            deadline,
            cause: None,
        })
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(std::convert::identity);
    match built {
        Ok(commit) => Some(commit),
        Err(error) => {
            owner_settled(chain);
            tracing::warn!(
                error = %crate::agent::diagnostics::error_class(&error),
                "ingress owner settlement could not be prepared; its input is left to recovery"
            );
            None
        }
    }
}

/// Record a run's pre-decision terminal (`turn.admission.failed`). When this
/// process recorded the run's input and still owns its row, the row is
/// reconciled in the same transaction (I3); otherwise the record is written
/// alone and the row, if any, is left to recovery.
pub(crate) async fn commit_admission_failure(
    fabric: &crate::memory::MemoryFabric,
    scope: crate::memory::MessageEventScope,
    summary: &str,
    payload: String,
    idempotency_key: String,
) -> anyhow::Result<crate::memory::MessageEvent> {
    let chain = match (scope.owner_id.as_deref(), scope.run_id.as_deref()) {
        (Some(owner_id), Some(run_id)) => Some(RecoveryChain {
            workspace_id: fabric.workspace_id().to_owned(),
            owner_id: owner_id.to_owned(),
            source: scope.source.as_str().to_owned(),
            run_id: run_id.to_owned(),
        }),
        _ => None,
    };
    if let Some(chain) = chain {
        let memory = fabric.memory();
        let deadline = Instant::now() + std::time::Duration::from_secs(15);
        if let Some(ingress) = owner_settlement(&memory, &chain, deadline).await {
            let input = fabric.runtime_event_input(scope, "turn.admission.failed", summary, payload, idempotency_key);
            let event = memory.commit_owned_admission_failure(input, ingress).await?;
            owner_settled(&chain);
            return Ok(event);
        }
    }
    fabric
        .record_runtime_event_idempotent(scope, "turn.admission.failed", summary, Some(payload), idempotency_key)
        .await
}

/// The run's terminal committed: its input is no longer offered for settlement.
pub(crate) fn owner_settled(chain: &RecoveryChain) {
    owned_inputs().lock().retain(|entry| entry.chain != *chain);
}

/// Rows already handled in this process, keyed by backend namespace and input.
#[derive(Default)]
struct RowMemo {
    quarantined: HashSet<(String, String)>,
    logged: HashSet<(String, String, &'static str)>,
}

/// Bound on each memo set. Past it a quarantined row is retried and an
/// outcome is not logged again.
const MAX_REMEMBERED_ROWS: usize = 4096;

fn row_memo() -> &'static parking_lot::Mutex<RowMemo> {
    static MEMO: std::sync::OnceLock<parking_lot::Mutex<RowMemo>> = std::sync::OnceLock::new();
    MEMO.get_or_init(Default::default)
}

fn row_key(namespace: &str, capsule: &IngressCapsule) -> (String, String) {
    (namespace.to_owned(), capsule.input_event_id.clone())
}

/// A row quarantined earlier in this process is not retried until a restart.
pub(super) fn quarantined_this_process(namespace: &str, capsule: &IngressCapsule) -> bool {
    row_memo().lock().quarantined.contains(&row_key(namespace, capsule))
}

/// Remember `outcome` and log it once per row and outcome, without payload.
/// An unchanged row is not logged: a later process revisits it on every scan
/// (to upgrade it once its final appears), and nothing about it changed.
pub(super) fn note_outcome(namespace: &str, capsule: &IngressCapsule, outcome: RecoveryRowOutcome) {
    if outcome == RecoveryRowOutcome::Unchanged {
        return;
    }
    let (namespace, input) = row_key(namespace, capsule);
    let first = {
        let mut memo = row_memo().lock();
        if matches!(outcome, RecoveryRowOutcome::Quarantined(_)) && memo.quarantined.len() < MAX_REMEMBERED_ROWS {
            memo.quarantined.insert((namespace.clone(), input.clone()));
        }
        memo.logged.len() < MAX_REMEMBERED_ROWS && memo.logged.insert((namespace, input, outcome.as_str()))
    };
    if first {
        tracing::warn!(
            outcome = outcome.as_str(),
            cause = outcome.cause(),
            run_id = %crate::agent::diagnostics::text_class(&capsule.run_id),
            "prior ingress row handled; new input continues"
        );
    }
}
