//! Authenticated evidence of one historical approval resume.
//!
//! Minted only by `AdmittedDecisionResume::gate_evidence` after every check
//! passes (scope and expiry, model, parity, the single capability's id,
//! schema and generation, and `verify_for_decision` per grant). The only
//! constructor takes a zero-sized witness whose constructor is private to
//! that module, so the value is authenticated by construction. It carries
//! data only: no serde, no public field.

use std::sync::Arc;

use crate::security::approval_ledger::historical::ResumeEvidenceWitness;
use crate::tools::capability::contract::{CapabilityRef, CapabilityTarget};

pub(crate) struct VerifiedResumeEvidenceV1 {
    call_digest: [u8; 32],
    capability: CapabilityRef,
    family: CapabilityTarget,
    namespace: String,
    grants: Vec<Arc<str>>,
    causation: Arc<str>,
    original_request: Arc<str>,
}

/// The verified parts of one resume, in the order `from_verified` takes them.
pub(crate) struct VerifiedResumeParts {
    pub(crate) call_digest: [u8; 32],
    pub(crate) capability: CapabilityRef,
    pub(crate) family: CapabilityTarget,
    pub(crate) namespace: String,
    pub(crate) grants: Vec<Arc<str>>,
    pub(crate) causation: Arc<str>,
    pub(crate) original_request: Arc<str>,
}

impl VerifiedResumeEvidenceV1 {
    pub(crate) fn from_verified(parts: VerifiedResumeParts, _witness: ResumeEvidenceWitness) -> Self {
        let VerifiedResumeParts {
            call_digest,
            capability,
            family,
            namespace,
            grants,
            causation,
            original_request,
        } = parts;
        Self {
            call_digest,
            capability,
            family,
            namespace,
            grants,
            causation,
            original_request,
        }
    }

    /// The pending binding's exact call digest.
    pub(crate) const fn call_digest(&self) -> &[u8; 32] {
        &self.call_digest
    }

    /// The single resumed catalog entry (id, schema fingerprint, generation).
    pub(crate) const fn capability(&self) -> &CapabilityRef {
        &self.capability
    }

    pub(crate) const fn family(&self) -> CapabilityTarget {
        self.family
    }

    /// The entry namespace the runtime key binds into the candidate id.
    pub(crate) fn namespace(&self) -> &str {
        &self.namespace
    }

    pub(crate) fn grants(&self) -> &[Arc<str>] {
        &self.grants
    }

    pub(crate) const fn causation(&self) -> &Arc<str> {
        &self.causation
    }

    /// Authenticated original source content, still untrusted decision data.
    pub(crate) const fn original_request(&self) -> &Arc<str> {
        &self.original_request
    }
}
