//! Shared execution boundary for owner-isolated operations HTTP adapters.

use super::ApiPrincipal;
use crate::tools::{
    Tool, ToolExecutionAuditRecord, ToolExecutionAuditSink, ToolExecutionCommand, ToolExecutionContext,
    ToolExecutionOutcome, ToolExecutionService, ToolExecutionStatus,
};
use axum::http::StatusCode;
use serde_json::Value;
use std::sync::Arc;

struct OperationsAudit {
    logger: Option<crate::security::audit::AuditLogger>,
    owner_id: String,
}

impl ToolExecutionAuditSink for OperationsAudit {
    fn record(&self, record: ToolExecutionAuditRecord) {
        let mut trace = record.clone();
        trace.error = trace.error.map(|_| format!("{:?}", trace.status));
        crate::tools::TracingToolExecutionAudit.record(trace);
        if let Some(logger) = &self.logger {
            // Only capability, terminal status and authenticated actor are persisted.
            // Arguments, tool output and error payloads can contain private memory.
            let mut event = crate::security::audit::AuditEvent::new(crate::security::audit::AuditEventType::ToolGate)
                .with_actor("console".into(), Some(self.owner_id.clone()), None)
                .with_action(
                    record.capability,
                    format!("{:?}", record.effect),
                    false,
                    record.decision == Some(crate::tools::ToolExecutionDecision::Allow),
                )
                .with_result(
                    record.status == ToolExecutionStatus::Succeeded,
                    None,
                    record.duration_ms,
                    (record.status != ToolExecutionStatus::Succeeded).then(|| format!("{:?}", record.status)),
                );
            event.event_id = record.operation_id;
            if logger.log(&event).is_err() {
                tracing::warn!("operations tool audit could not be persisted");
            }
        }
    }
}

pub(super) async fn execute_operation(
    config: &crate::config::Config,
    principal: &ApiPrincipal,
    tool: Box<dyn Tool>,
    name: &str,
    args: Value,
) -> ToolExecutionOutcome {
    let envelope = crate::runtime::envelope::RuntimeEnvelope::gateway(
        config.workspace_dir.to_string_lossy().into_owned(),
        format!("console:{}", principal.owner_id),
        "console",
        principal.owner_id.clone(),
        principal.owner_id.clone(),
        crate::memory::MemoryVisibility::Private,
    )
    .with_owner_id(principal.owner_id.clone())
    .with_owner_isolation();
    let audit = OperationsAudit {
        logger: crate::security::audit::AuditLogger::new(&config.security.audit, config.workspace_dir.clone())
            .map_err(|_| tracing::warn!("operations tool audit could not be initialized"))
            .ok(),
        owner_id: principal.owner_id.clone(),
    };
    let service = ToolExecutionService::from_boxed(
        vec![tool],
        Arc::new(crate::tools::SecurityEffectPolicy::new(
            crate::runtime::bootstrap::build_security_policy(config),
        )),
        Arc::new(crate::tools::DenyApprovalStrategy),
        Arc::new(crate::tools::AdapterOwnedPreparation),
        Arc::new(audit),
    );
    service
        .execute(
            ToolExecutionCommand::new(name, args),
            ToolExecutionContext::new(envelope, "private").with_dispatch_origin(crate::tools::DispatchOrigin::Operator),
            None,
        )
        .await
}

pub(super) fn result(outcome: ToolExecutionOutcome) -> Result<crate::tools::ToolResult, (StatusCode, &'static str)> {
    if let Some(result) = outcome.result {
        return Ok(result);
    }
    Err(match outcome.status {
        ToolExecutionStatus::Denied => (StatusCode::FORBIDDEN, "tool execution denied by policy"),
        ToolExecutionStatus::ApprovalDenied | ToolExecutionStatus::AwaitingApproval => {
            (StatusCode::FORBIDDEN, "explicit approval grant required")
        }
        ToolExecutionStatus::InvalidArguments => (StatusCode::BAD_REQUEST, "tool arguments are invalid"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "tool execution failed"),
    })
}
