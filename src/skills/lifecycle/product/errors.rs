use super::{ExperimentProductError, ExperimentProductFailureKind as Kind};
use crate::skills::lifecycle::experiment::ExperimentError;
use crate::skills::lifecycle::{GateError, LifecycleError, ServiceError};

fn lifecycle_kind(error: &LifecycleError) -> Kind {
    match error {
        LifecycleError::SourceReviewRequired { .. } => Kind::SourceReviewRequired,
        LifecycleError::OwnerMismatch { .. } => Kind::WrongOwner,
        LifecycleError::LockTimeout { .. } => Kind::Busy,
        LifecycleError::StaleGeneration { .. } | LifecycleError::RevisionAlreadyActive(_) => Kind::Stale,
        LifecycleError::TransactionConflict { .. } | LifecycleError::ProposalConflict { .. } => Kind::Conflict,
        LifecycleError::CommittedRepairRequired { .. } => Kind::RepairRequired,
        LifecycleError::TransactionNotCommitted { .. } => Kind::Uncertain,
        LifecycleError::InvalidId { .. } | LifecycleError::InvalidOwnerScope(_) => Kind::InvalidInput,
        _ => Kind::EvidenceUnavailable,
    }
}
fn gate_kind(error: &GateError) -> Kind {
    match error {
        GateError::Lifecycle(error) => lifecycle_kind(error),
        GateError::Stale(_) | GateError::Expired(_) | GateError::SuiteChanged(_) => Kind::Stale,
        GateError::Unsupported(_) => Kind::Unsupported,
        GateError::Unbound(_) | GateError::Tampered(_) => Kind::EvidenceUnavailable,
        _ => Kind::NotEligible,
    }
}
impl From<LifecycleError> for ExperimentProductError {
    fn from(error: LifecycleError) -> Self {
        Self::refused(lifecycle_kind(&error), error.to_string())
    }
}
impl From<GateError> for ExperimentProductError {
    fn from(error: GateError) -> Self {
        Self::refused(gate_kind(&error), error.to_string())
    }
}
impl From<ServiceError> for ExperimentProductError {
    fn from(error: ServiceError) -> Self {
        let kind = match &error {
            ServiceError::Disabled => Kind::Disabled,
            ServiceError::ModeForbids { .. } => Kind::Unsupported,
            ServiceError::Busy { .. } => Kind::Busy,
            ServiceError::NoSuchProposal(_) | ServiceError::NoGateDecision(_) => Kind::MissingAdmission,
            ServiceError::Lifecycle(error) => lifecycle_kind(error),
            ServiceError::Gate(error) => gate_kind(error),
            _ => Kind::NotEligible,
        };
        Self::refused(kind, error.to_string())
    }
}
impl From<ExperimentError> for ExperimentProductError {
    fn from(error: ExperimentError) -> Self {
        let kind = match &error {
            ExperimentError::Lifecycle(error) => lifecycle_kind(error),
            ExperimentError::Conflict => Kind::Conflict,
            ExperimentError::Expired => Kind::Stale,
            ExperimentError::InvalidSpec(_) => Kind::InvalidInput,
            _ => Kind::EvidenceUnavailable,
        };
        Self::refused(kind, error.to_string())
    }
}
impl From<anyhow::Error> for ExperimentProductError {
    fn from(error: anyhow::Error) -> Self {
        if let Some(inner) = error.downcast_ref::<ServiceError>() {
            let kind = match inner {
                ServiceError::Disabled => Kind::Disabled,
                ServiceError::Busy { .. } => Kind::Busy,
                ServiceError::Lifecycle(error) => lifecycle_kind(error),
                ServiceError::Gate(error) => gate_kind(error),
                _ => Kind::EvidenceUnavailable,
            };
            return Self::refused(kind, error.to_string());
        }
        let kind = error
            .downcast_ref::<LifecycleError>()
            .map(lifecycle_kind)
            .or_else(|| error.downcast_ref::<GateError>().map(gate_kind))
            .unwrap_or(Kind::EvidenceUnavailable);
        Self::refused(kind, error.to_string())
    }
}
