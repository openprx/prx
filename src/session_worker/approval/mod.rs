//! Approval relay for process-mode children.
//!
//! A worker below Full cannot ask anyone itself. It sends an authenticated
//! `ApprovalRequest` frame; the parent carries it to the approval surface of
//! the root session and answers with an authenticated `ApprovalDecision`. The
//! worker never creates a grant: it only passes the parent's verified grant to
//! the policy gate that already checks it for an in-process child.
pub(crate) mod client;
pub(crate) mod demux;
pub(crate) mod relay;
pub(crate) mod relay_context;
pub(crate) mod waiters;

use crate::tools::ApprovalDenialCause;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Longest public tool name a request may carry.
pub(crate) const MAX_TOOL_BYTES: usize = 64;
/// Longest tool call id a request may carry.
pub(crate) const MAX_CALL_ID_BYTES: usize = 256;
/// Largest serialized argument document a request may carry.
pub(crate) const MAX_ARGUMENT_BYTES: usize = 256 * 1024;
/// Most requests of one run that may wait at the same time.
pub(crate) const MAX_OUTSTANDING: usize = 8;

/// A request that is outside the relay bounds, or that does not hash to the
/// digest it claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ApprovalFrameError {
    #[error("approval request tool name is empty or too long")]
    Tool,
    #[error("approval request call id is empty or too long")]
    CallId,
    #[error("approval request arguments exceed the relay limit")]
    Arguments,
    #[error("approval request digest does not match its arguments")]
    Digest,
}

/// Parent verdict on one relayed request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerApprovalOutcome {
    /// The grant is the parent's verified runtime grant, when the surface
    /// issues one. The worker still verifies it at the policy gate.
    Approved { grant: Option<Value> },
    Denied {
        cause: ApprovalDenialCause,
        reason: String,
    },
    Cancelled { reason: String },
}

/// Worker-to-parent request. Only `tool`, `tool_call_id` and `arguments` are
/// taken from it; the parent rebuilds everything else from what it sealed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalRequestPayload {
    /// Monotonic per run, starting at 1.
    pub request_id: u64,
    pub tool_call_id: String,
    /// Public tool name.
    pub tool: String,
    pub arguments: Value,
    pub input_sha256: String,
}

/// Parent-to-worker answer, bound to one request by id and digest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalDecisionPayload {
    pub request_id: u64,
    pub input_sha256: String,
    /// Ledger call id of the pending approval, when the surface keeps one.
    pub approval_id: Option<String>,
    pub outcome: WorkerApprovalOutcome,
}

/// Digest that binds a decision to the exact call: the same formula the tool
/// pipeline uses for `input_sha256`.
#[must_use]
pub(crate) fn input_digest(arguments: &Value) -> String {
    format!("{:x}", Sha256::digest(arguments.to_string().as_bytes()))
}

/// Check the size bounds of one request.
pub(crate) fn check_bounds(tool: &str, call_id: &str, arguments: &Value) -> Result<(), ApprovalFrameError> {
    if tool.is_empty() || tool.len() > MAX_TOOL_BYTES {
        return Err(ApprovalFrameError::Tool);
    }
    if call_id.is_empty() || call_id.len() > MAX_CALL_ID_BYTES {
        return Err(ApprovalFrameError::CallId);
    }
    if arguments.to_string().len() > MAX_ARGUMENT_BYTES {
        return Err(ApprovalFrameError::Arguments);
    }
    Ok(())
}

impl ApprovalRequestPayload {
    /// Bounds, then the digest recomputed from the arguments actually received.
    pub(crate) fn validate(&self) -> Result<(), ApprovalFrameError> {
        check_bounds(&self.tool, &self.tool_call_id, &self.arguments)?;
        if input_digest(&self.arguments) != self.input_sha256 {
            return Err(ApprovalFrameError::Digest);
        }
        Ok(())
    }
}

/// Frame kinds that must only ever travel inside an authenticated mailbox frame.
#[must_use]
pub(crate) fn is_approval_kind(kind: &str) -> bool {
    matches!(kind, "approval_request" | "approval_withdraw" | "approval_decision")
}
