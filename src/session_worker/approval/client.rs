//! Worker-side approval strategy: relay one request to the parent and wait.
use super::{ApprovalRequestPayload, WorkerApprovalOutcome, check_bounds, input_digest, waiters};
use crate::session_worker::protocol::WorkerOutputFrame;
use crate::tools::{ApprovalDenialCause, ApprovalStrategy, ToolApprovalDecision, ToolApprovalRequest};

/// The approval strategy of a worker whose manifest enabled the relay.
///
/// It owns no timer: the parent always answers, and the registry resolves every
/// waiter as cancelled when stdin ends.
#[derive(Debug, Default)]
pub(crate) struct WorkerApprovalClient;

/// Sends `ApprovalWithdraw` when the waiting future is dropped before an
/// answer arrives (the segment was cancelled by steering or cancel).
struct WithdrawOnDrop {
    request_id: u64,
    armed: bool,
}

impl Drop for WithdrawOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        waiters::forget(self.request_id);
        if let Err(error) = crate::session_worker::stdout::frame(&WorkerOutputFrame::ApprovalWithdraw {
            request_id: self.request_id,
        }) {
            tracing::warn!(
                error = %crate::agent::diagnostics::error_class(&error),
                "session-worker could not withdraw an approval request"
            );
        }
    }
}

fn cancelled(reason: &str) -> ToolApprovalDecision {
    ToolApprovalDecision::Cancelled {
        reason: reason.to_owned(),
    }
}

fn refused(reason: &str) -> ToolApprovalDecision {
    ToolApprovalDecision::Denied {
        cause: ApprovalDenialCause::LedgerRefused,
        reason: format!("Approval request was refused ({reason}). Do not retry this call"),
    }
}

fn decision_from(outcome: WorkerApprovalOutcome) -> ToolApprovalDecision {
    match outcome {
        WorkerApprovalOutcome::Approved { grant } => ToolApprovalDecision::Approved {
            runtime_approval_granted: true,
            runtime_grant: grant,
        },
        WorkerApprovalOutcome::Denied { cause, reason } => ToolApprovalDecision::Denied { cause, reason },
        WorkerApprovalOutcome::Cancelled { reason } => ToolApprovalDecision::Cancelled { reason },
    }
}

#[async_trait::async_trait]
impl ApprovalStrategy for WorkerApprovalClient {
    async fn resolve(&self, request: ToolApprovalRequest) -> ToolApprovalDecision {
        let tool = request.descriptor.public_name.as_str();
        let call_id = request.command.operation_id.as_str();
        let arguments = &request.command.arguments;
        if check_bounds(tool, call_id, arguments).is_err() {
            return refused("ledger_refused: request exceeds the relay bounds");
        }
        let input_sha256 = input_digest(arguments);
        let Some((request_id, outcome)) = waiters::register(&input_sha256) else {
            return cancelled("approval channel closed");
        };
        let mut guard = WithdrawOnDrop {
            request_id,
            armed: false,
        };
        let frame = WorkerOutputFrame::ApprovalRequest(ApprovalRequestPayload {
            request_id,
            tool_call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            arguments: arguments.clone(),
            input_sha256,
        });
        if let Err(error) = crate::session_worker::stdout::frame(&frame) {
            waiters::forget(request_id);
            tracing::warn!(
                error = %crate::agent::diagnostics::error_class(&error),
                "session-worker could not send an approval request"
            );
            return ToolApprovalDecision::Denied {
                cause: ApprovalDenialCause::NoApprovalSurface,
                reason: crate::tools::execution::APPROVAL_DENIED_NO_APPROVAL_SURFACE.to_owned(),
            };
        }
        guard.armed = true;
        // The wait is not worker silence: the parent holds the same pause.
        let _pause = crate::agent::idle::pause_for_approval();
        let answer = outcome.await;
        guard.armed = false;
        answer.map_or_else(|_| cancelled("approval channel closed"), decision_from)
    }
}
