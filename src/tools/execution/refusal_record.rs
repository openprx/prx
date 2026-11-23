//! Durable `tool.call.refused` rows and the operator notice for a call the
//! service refused before any approval prompt or reservation: the execution
//! policy's Deny, or a tool's below-Full path precheck.
//!
//! Like `tool.approval.resolved`, each refusal gets a fresh id in its event
//! key, the row is payload-free (no path, argument or content), and a failed
//! write is logged without changing the outcome: the call stays refused. The
//! row is its own event type, so readers that count tool runs
//! (`tool.execution.finalized`) never count it.
use super::{
    ToolExecutionCommand, ToolExecutionContext, ToolExecutionService, tool_execution_event_input,
    tool_execution_scope_digest,
};
use crate::security::PathRefusalReason;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Event type of a pre-approval refusal row.
pub(crate) const TOOL_CALL_REFUSED_EVENT: &str = "tool.call.refused";

/// Closed reason a call was refused before approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallRefusalReason {
    OwnerIsolation,
    InvalidArguments,
    UnknownTool,
    StaleCapability,
    ServiceUnavailable,
    BarrierTimeout,
    PlanOnly,
    PolicyDenied,
    PathInvalid,
    PathForbidden,
    PathOutsideWorkspace,
    PathSymlinkOutside,
    PathDynamicShellArgument,
    PathActiveShellSubstitution,
}

impl ToolCallRefusalReason {
    /// The stable snake_case code recorded in the row.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OwnerIsolation => "owner_isolation",
            Self::InvalidArguments => "invalid_arguments",
            Self::UnknownTool => "unknown_tool",
            Self::StaleCapability => "stale_capability",
            Self::ServiceUnavailable => "service_unavailable",
            Self::BarrierTimeout => "barrier_timeout",
            Self::PlanOnly => "plan_only",
            Self::PolicyDenied => "policy_denied",
            Self::PathInvalid => "path_invalid",
            Self::PathForbidden => "path_forbidden",
            Self::PathOutsideWorkspace => "path_outside_workspace",
            Self::PathSymlinkOutside => "path_symlink_outside",
            Self::PathDynamicShellArgument => "path_dynamic_shell_argument",
            Self::PathActiveShellSubstitution => "path_active_shell_substitution",
        }
    }
}

impl From<PathRefusalReason> for ToolCallRefusalReason {
    fn from(reason: PathRefusalReason) -> Self {
        match reason {
            PathRefusalReason::InvalidPath => Self::PathInvalid,
            PathRefusalReason::ForbiddenPath => Self::PathForbidden,
            PathRefusalReason::OutsideWorkspace => Self::PathOutsideWorkspace,
            PathRefusalReason::SymlinkEscape => Self::PathSymlinkOutside,
            PathRefusalReason::DynamicShellArgument => Self::PathDynamicShellArgument,
            PathRefusalReason::ActiveShellSubstitution => Self::PathActiveShellSubstitution,
        }
    }
}

/// Operator surface for a pre-approval refusal: plain mode prints one stderr
/// line, the TUI a system notice. `text` is the refusal text the model also
/// receives; it may name the path, so it is never logged.
#[async_trait::async_trait]
pub(crate) trait ToolRefusalNotice: Send + Sync {
    async fn refused(&self, operation_id: &str, tool: &str, text: &str);
}

/// Payload of one refusal row.
#[derive(Debug, Serialize)]
struct ToolCallRefusedRecord<'a> {
    operation_id: &'a str,
    capability: &'a str,
    input_sha256: &'a str,
    reason: ToolCallRefusalReason,
    cause: &'static str,
}

impl ToolExecutionService {
    /// Tell the operator, then await one attempt to record the refusal.
    pub(super) async fn record_call_refusal(
        &self,
        command: &ToolExecutionCommand,
        context: &ToolExecutionContext,
        input_sha256: &str,
        reason: ToolCallRefusalReason,
        text: &str,
    ) {
        if let Some(notice) = self.refusal_notice.as_ref() {
            notice.refused(&command.operation_id, &command.name, text).await;
        }
        let Some(memory) = self.idempotency_memory.as_ref() else {
            tracing::warn!(
                reason = reason.as_str(),
                "tool call refusal not recorded: no durable tool ledger is attached"
            );
            return;
        };
        let scope_key = command.idempotency_key.as_deref().unwrap_or(&command.operation_id);
        let key = format!(
            "tool-refusal:{}:{}",
            tool_execution_scope_digest(context, scope_key),
            Uuid::now_v7()
        );
        let content = format!(
            "operation_id={} capability={} reason={}",
            command.operation_id,
            command.name,
            reason.as_str()
        );
        let record = ToolCallRefusedRecord {
            operation_id: &command.operation_id,
            capability: &command.name,
            input_sha256,
            reason,
            cause: crate::security::approval_ledger::run_call::rf_denial_cause(reason),
        };
        let payload = match serde_json::to_string(&record) {
            Ok(payload) => payload,
            Err(error) => {
                let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
                tracing::error!(
                    reason = reason.as_str(),
                    error_len,
                    error_sha256,
                    "tool call refusal not recorded: payload encoding failed"
                );
                return;
            }
        };
        let appended = memory
            .append_message_event(tool_execution_event_input(
                context,
                &key,
                TOOL_CALL_REFUSED_EVENT,
                content,
                payload,
            ))
            .await;
        crate::agent::idle::beat(crate::agent::idle::ProgressKind::DurableWrite);
        if let Err(error) = appended {
            let (error_len, error_sha256) = crate::agent::diagnostics::error_log_digest(&error);
            tracing::error!(
                reason = reason.as_str(),
                error_len,
                error_sha256,
                "tool call refusal not recorded: the ledger write failed"
            );
        }
    }
}
