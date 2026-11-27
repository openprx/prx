//! K-H3: the ledger row of a call whose `tool_result` hook replaced its output.
//!
//! The terminal row stores the tool output as the service produced it. When a
//! hook substituted the text the model reads, the row is rewritten to the
//! effective text (masked for secrets) so a retry of the same operation replays
//! what the model saw. A failed or absent row changes nothing.

use super::{ToolExecutionContext, ToolExecutionService, tool_execution_ledger_keys};

impl ToolExecutionService {
    /// Rewrite the output fields of the terminal ledger row of the call keyed
    /// by `idempotency_key` to `effective`.
    pub(crate) async fn rewrite_ledger_effective_output(
        &self,
        context: &ToolExecutionContext,
        idempotency_key: &str,
        effective: &str,
    ) {
        let Some(memory) = self.idempotency_memory.as_ref() else {
            return;
        };
        let (_, terminal_key) = tool_execution_ledger_keys(context, idempotency_key);
        let masked = crate::security::redaction::redact_secrets(effective);
        if let Err(error) = memory
            .rewrite_tool_ledger_output(&context.envelope.workspace_id, &terminal_key, &masked)
            .await
        {
            tracing::warn!(error = %error, "failed to rewrite the tool ledger row with the effective output");
        }
    }
}
