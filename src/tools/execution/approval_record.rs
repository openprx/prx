//! Durable `tool.approval.resolved` rows: one per approval resolution.
//!
//! Each resolution gets a fresh resolution id in its event key, so two calls
//! that share an execution idempotency key (the scope digest excludes the
//! operation identity and the input) still leave two observable rows. The
//! execution-ledger keys are unchanged. A failed write is logged and never
//! changes the decision: a denial stays denied, and an approved call proceeds
//! to its reservation, which remains the execution gate.
use super::{
    ApprovalDenialCause, ApprovalVerdict, ToolExecutionCommand, ToolExecutionContext, ToolExecutionService,
    tool_execution_event_input, tool_execution_scope_digest,
};
use serde::Serialize;
use uuid::Uuid;

/// Event type of an approval resolution row.
pub(crate) const TOOL_APPROVAL_RESOLVED_EVENT: &str = "tool.approval.resolved";

/// Payload of one approval resolution row.
#[derive(Debug, Serialize)]
struct ToolApprovalRecord<'a> {
    operation_id: &'a str,
    capability: &'a str,
    input_sha256: &'a str,
    verdict: ApprovalVerdict,
    cause: Option<ApprovalDenialCause>,
    reason: Option<&'a str>,
}

impl ToolExecutionService {
    /// Await one attempt to record how this call's approval was resolved.
    pub(super) async fn record_approval_resolution(
        &self,
        command: &ToolExecutionCommand,
        context: &ToolExecutionContext,
        input_sha256: &str,
        verdict: ApprovalVerdict,
        cause: Option<ApprovalDenialCause>,
        reason: Option<&str>,
    ) {
        let Some(memory) = self.idempotency_memory.as_ref() else {
            tracing::warn!(
                verdict = verdict.as_str(),
                "tool approval resolution not recorded: no durable tool ledger is attached"
            );
            return;
        };
        let scope_key = command.idempotency_key.as_deref().unwrap_or(&command.operation_id);
        let key = format!(
            "tool-approval:{}:{}",
            tool_execution_scope_digest(context, scope_key),
            Uuid::now_v7()
        );
        let mut content = format!(
            "operation_id={} capability={} verdict={}",
            command.operation_id,
            command.name,
            verdict.as_str()
        );
        if let Some(cause) = cause {
            content.push_str(" cause=");
            content.push_str(cause.as_str());
        }
        let record = ToolApprovalRecord {
            operation_id: &command.operation_id,
            capability: &command.name,
            input_sha256,
            verdict,
            cause,
            reason,
        };
        let payload = match serde_json::to_string(&record) {
            Ok(payload) => payload,
            Err(error) => {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                tracing::error!(
                    verdict = verdict.as_str(),
                    error_len,
                    error_sha256,
                    "tool approval resolution not recorded: payload encoding failed"
                );
                return;
            }
        };
        let appended = memory
            .append_message_event(tool_execution_event_input(
                context,
                &key,
                TOOL_APPROVAL_RESOLVED_EVENT,
                content,
                payload,
            ))
            .await;
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::DurableWrite);
        if let Err(error) = appended {
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::error!(
                verdict = verdict.as_str(),
                error_len,
                error_sha256,
                "tool approval resolution not recorded: the ledger write failed"
            );
        }
    }
}
