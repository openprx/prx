//! The closed class of a failed Gateway turn.
//!
//! `POST /webhook` answers "LLM request failed" only for a failed provider
//! request. Every other failure answers `{"error":"turn_failed","reason":<code>}`
//! where the code names the step that failed, never the error text: a storage,
//! admission or terminal failure is not the provider's, and the caller (and the
//! operator reading the log) needs to tell them apart.

/// The step of a Gateway turn that was running when it failed. Closed: each
/// variant is one wire `reason` value and one log `reason` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TurnFailureClass {
    /// The provider chain ran out of candidates: the only class reported as
    /// "LLM request failed".
    Provider,
    /// The turn runtime refused the turn at its start.
    TurnNotReady,
    /// The provider binding was missing or did not match the runtime.
    ProviderBinding,
    /// The durable oversized-reply lookup failed.
    ReplayLookup,
    /// A gateway resource mutation was denied.
    ResourceDenied,
    /// Recording the inbound input (and registering its cleanup) failed.
    InputRecord,
    /// Binding the recorded source to the turn runtime failed.
    SourceBinding,
    /// Decision admission (cleanup scope, publication, turn context) failed.
    DecisionAdmission,
    /// Binding the terminal to the admitted decision failed.
    TerminalBinding,
    /// The provider cannot serve the input (a capability it lacks).
    ProviderCapability,
    /// Preparing the turn's prompt failed.
    TurnPreparation,
    /// The tool loop failed for a reason other than its provider.
    TurnLoop,
    /// The turn was cancelled.
    Cancelled,
    /// The turn's owned cleanup did not settle.
    TurnCleanup,
    /// Terminal persistence: the store refused its recovery key.
    RecoveryKeyUnavailable,
    /// Terminal persistence: the auxiliary receipts failed.
    AuxiliaryReceiptFailed,
    /// Terminal persistence: the descendant drain did not complete.
    ChildCleanupIncomplete,
    /// Terminal persistence: the canonical terminal commit failed.
    TerminalCommitFailed,
    /// Terminal persistence: the usage record failed.
    UsageRecordFailed,
    /// Terminal persistence failed before any persistence step.
    TerminalPersistenceFailed,
    /// A broken internal invariant.
    Internal,
}

impl TurnFailureClass {
    /// The wire and log value of this class.
    pub(super) const fn code(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::TurnNotReady => "turn_not_ready",
            Self::ProviderBinding => "provider_binding",
            Self::ReplayLookup => "replay_lookup",
            Self::ResourceDenied => "resource_denied",
            Self::InputRecord => "input_record",
            Self::SourceBinding => "source_binding",
            Self::DecisionAdmission => "decision_admission",
            Self::TerminalBinding => "terminal_binding",
            Self::ProviderCapability => "provider_capability",
            Self::TurnPreparation => "turn_preparation",
            Self::TurnLoop => "turn_loop",
            Self::Cancelled => "cancelled",
            Self::TurnCleanup => "turn_cleanup",
            Self::RecoveryKeyUnavailable => "recovery_key_unavailable",
            Self::AuxiliaryReceiptFailed => "auxiliary_receipt_failed",
            Self::ChildCleanupIncomplete => "child_cleanup_incomplete",
            Self::TerminalCommitFailed => "terminal_commit_failed",
            Self::UsageRecordFailed => "usage_record_failed",
            Self::TerminalPersistenceFailed => "terminal_persistence_failed",
            Self::Internal => "internal",
        }
    }
}

impl std::fmt::Display for TurnFailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl From<crate::runtime::turn_terminal::TerminalPersistReason> for TurnFailureClass {
    fn from(reason: crate::runtime::turn_terminal::TerminalPersistReason) -> Self {
        use crate::runtime::turn_terminal::TerminalPersistReason;
        match reason {
            TerminalPersistReason::RecoveryKeyUnavailable => Self::RecoveryKeyUnavailable,
            TerminalPersistReason::AuxiliaryReceiptFailed => Self::AuxiliaryReceiptFailed,
            TerminalPersistReason::ChildCleanupIncomplete => Self::ChildCleanupIncomplete,
            TerminalPersistReason::TerminalCommitFailed => Self::TerminalCommitFailed,
            TerminalPersistReason::UsageRecordFailed => Self::UsageRecordFailed,
            TerminalPersistReason::Unclassified => Self::TerminalPersistenceFailed,
        }
    }
}

impl TurnFailureClass {
    /// Classify `error`, returned while `stage` was running.
    ///
    /// A provider failure is recognised by its type wherever it sits in the
    /// chain: the gateway's provider is always a resilient chain
    /// (`providers::create_provider_for`), whose exhaustion is a
    /// [`crate::providers::reliable::ProviderChainFailure`]. A typed error
    /// that names its own class wins over the stage.
    pub(super) fn classify(stage: Self, error: &anyhow::Error) -> Self {
        let mut typed = None;
        for cause in error.chain() {
            if cause.is::<crate::providers::reliable::ProviderChainFailure>() {
                return Self::Provider;
            }
            if typed.is_none() {
                typed = if cause.is::<crate::providers::ProviderCapabilityError>() {
                    Some(Self::ProviderCapability)
                } else if cause.is::<super::TurnNotReady>() {
                    Some(Self::TurnNotReady)
                } else if cause.is::<crate::memory::recovery_authority::RecoveryKeyUnavailable>() {
                    Some(Self::RecoveryKeyUnavailable)
                } else {
                    None
                };
            }
        }
        typed.unwrap_or(stage)
    }
}
