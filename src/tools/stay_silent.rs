//! `stay_silent` — let the model decline to reply to a group message.
//!
//! In smart group-reply mode the bot reads every group message (not just
//! @-mentions). For most of them the right behavior is to say nothing — like a
//! real participant who only speaks when they have something relevant to add.
//! This tool gives the model a structured way to express that decision.
//!
//! ## Terminal semantics
//!
//! Calling `stay_silent` is a *terminal* decision for the turn: the tool loop
//! (`run_tool_call_loop_outcome`) recognizes the call by name and returns
//! [`ToolLoopOutcome::Silent`](crate::agent::loop_) **without executing the tool
//! and without writing any assistant/tool history**, so the silent turn leaves
//! no trace in the conversation history and nothing is sent to the channel.
//!
//! Because the loop short-circuits before execution, [`Tool::execute`] here is a
//! defensive fallback only — it is reached solely if the tool is ever invoked
//! outside the loop's short-circuit (it returns a benign, empty acknowledgement
//! and never errors, so it can never break a turn).
//!
//! ## Exposure
//!
//! The tool name constant is the single source of truth shared with the loop's
//! short-circuit detection. The tool is only *advertised* to the model in smart
//! group context (the loop filters its spec out otherwise); see
//! `run_tool_call_loop_outcome`'s `expose_stay_silent` gate. DMs and non-smart
//! modes never expose it, which is one of the three hard-coded "DM never stays
//! silent" guarantees.

use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use async_trait::async_trait;
use serde_json::json;

/// Public tool name. Single source of truth shared with the loop's
/// short-circuit detection so a rename stays consistent across producer
/// (this tool's spec) and consumer (the loop).
pub const STAY_SILENT_TOOL_NAME: &str = "stay_silent";

/// Maximum reason length retained for logging/metrics. The reason is operator-
/// and machine-facing only (never sent to a channel), so it is bounded to avoid
/// unbounded log lines.
pub const STAY_SILENT_REASON_MAX_CHARS: usize = 280;

/// Extract and normalize the `reason` argument from a `stay_silent` tool call.
///
/// Always succeeds: a missing/empty/non-string reason collapses to a stable
/// default so the silent decision is never blocked by a malformed argument.
/// The result is trimmed and length-bounded for logging.
pub fn extract_reason(arguments: &serde_json::Value) -> String {
    let raw = arguments
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim();
    let reason = if raw.is_empty() { "no reason provided" } else { raw };
    reason.chars().take(STAY_SILENT_REASON_MAX_CHARS).collect()
}

/// Tool that lets the model intentionally stay silent on a group message.
#[derive(Default)]
pub struct StaySilentTool;

impl StaySilentTool {
    /// Construct a new `stay_silent` tool.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for StaySilentTool {
    fn name(&self) -> &str {
        STAY_SILENT_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Decline to reply to the current group message. Call this when the message is \
         not directed at you, is small talk between other people, or otherwise does not \
         need a response from you. Calling it ends your turn with no message sent. \
         Provide a short `reason` describing why staying silent is appropriate."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "reason": {
                    "type": "string",
                    "description": "Short explanation of why no reply is needed (for logs/metrics, never sent to the chat)."
                }
            },
            "required": ["reason"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Defensive fallback only — the tool loop short-circuits on this tool's
        // name before execution, so this path is normally unreachable. Return a
        // benign, non-error acknowledgement so an out-of-loop invocation cannot
        // break a turn.
        let reason = extract_reason(&args);
        Ok(ToolResult {
            success: true,
            output: format!("(stayed silent: {reason})"),
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        // Extended: only surfaced when explicitly relevant. The loop additionally
        // gates advertising it to smart group turns only.
        ToolTier::Extended
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::Communication]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_constant() {
        let tool = StaySilentTool::new();
        assert_eq!(tool.name(), STAY_SILENT_TOOL_NAME);
        assert_eq!(tool.name(), "stay_silent");
    }

    #[test]
    fn schema_requires_reason() {
        let tool = StaySilentTool::new();
        let schema = tool.parameters_schema();
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .expect("required array");
        assert_eq!(required.first().and_then(|v| v.as_str()), Some("reason"));
    }

    #[test]
    fn extract_reason_uses_default_when_missing() {
        assert_eq!(extract_reason(&json!({})), "no reason provided");
        assert_eq!(extract_reason(&json!({ "reason": "   " })), "no reason provided");
    }

    #[test]
    fn extract_reason_trims_and_bounds() {
        assert_eq!(extract_reason(&json!({ "reason": "  small talk  " })), "small talk");
        let long = "x".repeat(STAY_SILENT_REASON_MAX_CHARS + 50);
        assert_eq!(
            extract_reason(&json!({ "reason": long })).chars().count(),
            STAY_SILENT_REASON_MAX_CHARS
        );
    }

    #[tokio::test]
    async fn execute_never_errors() {
        let tool = StaySilentTool::new();
        let result = tool.execute(json!({ "reason": "off-topic" })).await.unwrap();
        assert!(result.success);
        assert!(result.error.is_none());
        assert!(result.output.contains("off-topic"));
    }
}
