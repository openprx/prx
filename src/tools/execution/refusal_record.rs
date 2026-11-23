//! Durable `tool.call.refused` rows and the existing operator refusal notice.
//! Pre-approval policy/path refusals record here; rf also records scheduler
//! refusals and failed outcomes before publishing their live frame.
//!
//! Like `tool.approval.resolved`, each refusal gets a fresh id in its event
//! key, the row is payload-free (no path, argument or content), and a failed
//! write is logged without changing the outcome: the call stays refused. The
//! row is its own event type. It does not prove backend entry or execution;
//! readers that count tool runs still use `tool.execution.finalized`.
use super::{
    ToolExecutionCommand, ToolExecutionContext, ToolExecutionService, tool_execution_event_input,
    tool_execution_scope_digest,
};
use crate::security::PathRefusalReason;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use uuid::Uuid;

/// Event type of a refusal or unsuccessful rf tool outcome.
pub(crate) const TOOL_CALL_REFUSED_EVENT: &str = "tool.call.refused";

/// Closed code for a refusal or unsuccessful rf tool outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallRefusalReason {
    OwnerIsolation,
    ToolNotExposed,
    Failed,
    Indeterminate,
    PreparationDenied,
    IdempotencyConflict,
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
            Self::ToolNotExposed => "tool_not_exposed",
            Self::Failed => "failed",
            Self::Indeterminate => "indeterminate",
            Self::PreparationDenied => "preparation_denied",
            Self::IdempotencyConflict => "idempotency_conflict",
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
    reason: &'a str,
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
        // rf records every denied/failed frame at the scheduler boundary,
        // including refusals that never enter this service. Keep one row per
        // outcome there; the operator notice above retains its original order.
        // The worker's sealed refusal inventory uses that same scheduler
        // boundary. Approval calls still stay denied with no_resolver.
        if context.live_turn.is_some()
            || (context.pending_approval_denied
                && context.envelope.source == crate::runtime::envelope::RuntimeSource::SessionWorker)
        {
            return;
        }
        let Some(memory) = self.idempotency_memory.as_ref() else {
            tracing::warn!(
                reason = reason.as_str(),
                "tool call refusal not recorded: no durable tool ledger is attached"
            );
            return;
        };
        if let Err(error) = append_refusal(
            memory,
            command,
            context,
            input_sha256,
            reason.as_str(),
            crate::security::approval_ledger::run_call::rf_denial_cause(reason),
        )
        .await
        {
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

/// Commit the existing refusal event before rf can publish a denied/failed
/// frame. Missing storage or a failed write fails the turn, never silently
/// emits an unrecorded refusal. Other ingress event sequences stay unchanged.
pub(crate) async fn record_rf_call_refusal(
    service: Option<&ToolExecutionService>,
    command: &ToolExecutionCommand,
    context: &ToolExecutionContext,
    cause: crate::security::approval_ledger::run_call::RfDenialCause,
) -> anyhow::Result<()> {
    let memory = service
        .and_then(|service| service.idempotency_memory.as_ref())
        .or_else(|| context.approval_ledger.as_ref().map(|ledger| &ledger.memory))
        .context("rf tool refusal has no durable ledger")?;
    let input_sha256 = format!("{:x}", sha2::Sha256::digest(command.arguments.to_string().as_bytes()));
    let reason = match cause {
        crate::security::approval_ledger::run_call::RfDenialCause::Approval(cause) => cause.as_str(),
        crate::security::approval_ledger::run_call::RfDenialCause::Refusal(reason) => reason.as_str(),
    };
    append_refusal(
        memory,
        command,
        context,
        &input_sha256,
        reason,
        crate::security::approval_ledger::run_call::rf_denial_cause(cause),
    )
    .await
}

async fn append_refusal(
    memory: &std::sync::Arc<dyn crate::memory::Memory>,
    command: &ToolExecutionCommand,
    context: &ToolExecutionContext,
    input_sha256: &str,
    reason: &str,
    cause: &'static str,
) -> anyhow::Result<()> {
    let scope_key = command.idempotency_key.as_deref().unwrap_or(&command.operation_id);
    let key = format!(
        "tool-refusal:{}:{}",
        tool_execution_scope_digest(context, scope_key),
        Uuid::now_v7()
    );
    let content = format!(
        "operation_id={} capability={} reason={}",
        command.operation_id, command.name, reason
    );
    let record = ToolCallRefusedRecord {
        operation_id: &command.operation_id,
        capability: &command.name,
        input_sha256,
        reason,
        cause,
    };
    let payload = serde_json::to_string(&record)?;
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
    appended.context("tool refusal could not be committed")?;
    Ok(())
}
