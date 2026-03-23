//! Build a [`CausalState`] snapshot from Agent runtime data.
//!
//! This module contains a single pure function, [`build_causal_state`], that
//! maps the Agent's in-flight request context into the immutable [`CausalState`]
//! value consumed by the rest of the Causal Tree Engine.

use super::state::{
    ArtifactRef, ArtifactSource, ArtifactType, BudgetState, CausalState, SideEffectMode,
    StepRecord, StepStatus,
};
use crate::agent::classifier::{ClassifyResult, TaskIntent};
use crate::providers::ConversationMessage;

/// Maximum number of UTF-8 characters kept for the `goal` field.
const GOAL_MAX_CHARS: usize = 512;

/// Truncate `s` to at most `max_chars` Unicode scalar values, always cutting at
/// a valid char boundary (never splits a multi-byte sequence).
fn truncate_to_chars(s: &str, max_chars: usize) -> &str {
    if s.chars().count() <= max_chars {
        return s;
    }
    // Find the byte offset of the (max_chars+1)-th character boundary.
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

/// Map the classified [`TaskIntent`] to a human-readable intent string.
fn intent_label(intent: TaskIntent) -> &'static str {
    match intent {
        TaskIntent::Simple => "simple",
        TaskIntent::Delegate => "delegate",
        TaskIntent::Stream => "stream",
    }
}

/// Derive active constraint strings from the requested [`SideEffectMode`].
fn constraints_for_mode(mode: SideEffectMode) -> Vec<String> {
    match mode {
        SideEffectMode::ReadOnly => vec!["no_write".to_string()],
        SideEffectMode::ApprovalRequired => vec!["approval_required".to_string()],
        SideEffectMode::GuardedWrite => vec![],
    }
}

/// Convert a single assistant [`ConversationMessage::Chat`] entry (role =
/// `"assistant"`) into a [`StepRecord`] with status [`StepStatus::Succeeded`].
///
/// The `index` parameter is used only to generate a stable, unique step ID
/// within this snapshot (e.g. `"step-3"`).
fn assistant_chat_to_step(content: &str, index: usize, ts: &str) -> StepRecord {
    // Truncate the label to at most 256 chars so it remains a concise summary.
    let label = truncate_to_chars(content, 256).to_string();
    StepRecord {
        step_id: format!("step-{index}"),
        label,
        status: StepStatus::Succeeded,
        started_at: ts.to_string(),
        ended_at: Some(ts.to_string()),
        evidence: vec![],
    }
}

/// Convert a single [`crate::providers::ToolResultMessage`] into an
/// [`ArtifactRef`] of type [`ArtifactType::ToolOutput`].
///
/// `_tool_call_id` is kept in the signature for future use (e.g. deduplication
/// or cross-referencing with the originating tool call).
fn tool_result_to_artifact(
    _tool_call_id: &str,
    content: &str,
    index: usize,
) -> ArtifactRef {
    let summary = truncate_to_chars(content, 256).to_string();
    ArtifactRef {
        artifact_id: format!("artifact-{index}"),
        artifact_type: ArtifactType::ToolOutput,
        summary,
        source: ArtifactSource::ToolExecution,
        // Default importance: tool outputs are moderately important (0.6).
        importance: 0.6,
    }
}

/// Build an immutable [`CausalState`] snapshot from Agent runtime inputs.
///
/// This function is **pure** — it does not mutate any external state and
/// always succeeds (no `Result` return value needed).
///
/// # Parameters
///
/// - `session_id`        — Stable session identifier (passed through).
/// - `user_message`      — Raw user message; truncated to [`GOAL_MAX_CHARS`]
///                         chars for the `goal` field.
/// - `classify_result`   — Output of the task classifier; drives `user_intent`.
/// - `history`           — Full conversation history for the current session.
///   - `Chat` messages with `role == "assistant"` become completed
///     [`StepRecord`]s.
///   - `ToolResults` entries become [`ArtifactRef`]s.
/// - `side_effect_mode`  — Current permission mode; controls `active_constraints`.
/// - `policy`            — Active [`CausalPolicy`]; used to set the
///                         [`BudgetState`].
pub fn build_causal_state(
    session_id: &str,
    user_message: &str,
    classify_result: &ClassifyResult,
    history: &[ConversationMessage],
    side_effect_mode: SideEffectMode,
    policy: &super::policy::CausalPolicy,
) -> CausalState {
    let request_id = uuid::Uuid::new_v4().to_string();
    let snapshot_ts = chrono::Utc::now().to_rfc3339();

    // goal: user_message truncated to GOAL_MAX_CHARS chars (char-boundary safe).
    let goal = truncate_to_chars(user_message, GOAL_MAX_CHARS).to_string();

    // user_intent: derived from the classifier result.
    let user_intent = intent_label(classify_result.intent).to_string();

    // completed_steps: one StepRecord per assistant Chat message.
    let mut step_index: usize = 0;
    let mut completed_steps: Vec<StepRecord> = Vec::new();

    // known_artifacts: one ArtifactRef per ToolResultMessage.
    let mut artifact_index: usize = 0;
    let mut known_artifacts: Vec<ArtifactRef> = Vec::new();

    for msg in history {
        match msg {
            ConversationMessage::Chat(chat) if chat.role == "assistant" => {
                completed_steps.push(assistant_chat_to_step(
                    &chat.content,
                    step_index,
                    &snapshot_ts,
                ));
                step_index += 1;
            }
            ConversationMessage::ToolResults(results) => {
                for result in results {
                    known_artifacts.push(tool_result_to_artifact(
                        &result.tool_call_id,
                        &result.content,
                        artifact_index,
                    ));
                    artifact_index += 1;
                }
            }
            // Chat(system/user) and AssistantToolCalls are not mapped in v1.
            ConversationMessage::Chat(_) | ConversationMessage::AssistantToolCalls { .. } => {}
        }
    }

    let active_constraints = constraints_for_mode(side_effect_mode);

    let budget = BudgetState {
        extra_token_limit: 4096,
        tokens_used: 0,
        extra_latency_budget_ms: policy.extra_latency_budget_ms,
        latency_used_ms: 0,
    };

    CausalState {
        session_id: session_id.to_string(),
        request_id,
        goal,
        user_intent,
        completed_steps,
        active_constraints,
        known_artifacts,
        unresolved_risks: Vec::new(),
        side_effect_mode,
        budget,
        snapshot_ts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::classifier::{ClassifyResult, TaskIntent};
    use crate::providers::{ChatMessage, ConversationMessage, ToolResultMessage};
    use super::super::policy::CausalPolicy;

    fn default_classify(intent: TaskIntent) -> ClassifyResult {
        ClassifyResult {
            intent,
            model_hint: None,
            reason: "test".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // Test 1: empty history — verify basic fields
    // -----------------------------------------------------------------------
    #[test]
    fn test_empty_history_basic_fields() {
        let policy = CausalPolicy::default();
        let state = build_causal_state(
            "sess-abc",
            "What is the capital of France?",
            &default_classify(TaskIntent::Simple),
            &[],
            SideEffectMode::ReadOnly,
            &policy,
        );

        assert_eq!(state.session_id, "sess-abc");
        assert_eq!(state.goal, "What is the capital of France?");
        assert_eq!(state.user_intent, "simple");
        assert!(state.completed_steps.is_empty());
        assert!(state.known_artifacts.is_empty());
        assert!(state.unresolved_risks.is_empty());
        assert_eq!(state.active_constraints, vec!["no_write"]);
        assert_eq!(state.budget.extra_token_limit, 4096);
        assert_eq!(state.budget.tokens_used, 0);
        assert_eq!(
            state.budget.extra_latency_budget_ms,
            policy.extra_latency_budget_ms,
        );
        assert_eq!(state.side_effect_mode, SideEffectMode::ReadOnly);
        // request_id must be a non-empty UUID string.
        assert!(!state.request_id.is_empty());
        // snapshot_ts must be a non-empty RFC-3339 string.
        assert!(!state.snapshot_ts.is_empty());
    }

    // -----------------------------------------------------------------------
    // Test 2: history with assistant messages and ToolResults
    //         → verify steps and artifacts extraction
    // -----------------------------------------------------------------------
    #[test]
    fn test_history_steps_and_artifacts() {
        let policy = CausalPolicy::default();

        let history = vec![
            ConversationMessage::Chat(ChatMessage::user("Fix the bug")),
            ConversationMessage::Chat(ChatMessage::assistant("Reading the file...")),
            ConversationMessage::ToolResults(vec![
                ToolResultMessage {
                    tool_call_id: "call-1".to_string(),
                    content: "file content here".to_string(),
                },
                ToolResultMessage {
                    tool_call_id: "call-2".to_string(),
                    content: "another result".to_string(),
                },
            ]),
            ConversationMessage::Chat(ChatMessage::assistant("Patch applied successfully.")),
        ];

        let state = build_causal_state(
            "sess-xyz",
            "Fix the bug in main.rs",
            &default_classify(TaskIntent::Delegate),
            &history,
            SideEffectMode::ApprovalRequired,
            &policy,
        );

        // Two assistant messages → two steps.
        assert_eq!(state.completed_steps.len(), 2);
        assert_eq!(state.completed_steps[0].step_id, "step-0");
        assert_eq!(state.completed_steps[0].label, "Reading the file...");
        assert_eq!(state.completed_steps[0].status, StepStatus::Succeeded);
        assert_eq!(state.completed_steps[1].step_id, "step-1");
        assert_eq!(state.completed_steps[1].label, "Patch applied successfully.");

        // Two ToolResultMessages → two artifacts.
        assert_eq!(state.known_artifacts.len(), 2);
        assert_eq!(state.known_artifacts[0].artifact_id, "artifact-0");
        assert_eq!(state.known_artifacts[0].artifact_type, ArtifactType::ToolOutput);
        assert_eq!(state.known_artifacts[0].source, ArtifactSource::ToolExecution);
        assert_eq!(state.known_artifacts[0].summary, "file content here");
        assert_eq!(state.known_artifacts[1].artifact_id, "artifact-1");
        assert_eq!(state.known_artifacts[1].summary, "another result");

        // ApprovalRequired → constraint.
        assert_eq!(state.active_constraints, vec!["approval_required"]);

        // Intent.
        assert_eq!(state.user_intent, "delegate");
    }

    // -----------------------------------------------------------------------
    // Test 3: long user_message → goal is truncated to ≤ GOAL_MAX_CHARS chars
    // -----------------------------------------------------------------------
    #[test]
    fn test_long_user_message_truncated() {
        let policy = CausalPolicy::default();

        // Build a message longer than GOAL_MAX_CHARS (512) characters.
        let long_msg: String = "A".repeat(1000);

        let state = build_causal_state(
            "sess-trunc",
            &long_msg,
            &default_classify(TaskIntent::Stream),
            &[],
            SideEffectMode::GuardedWrite,
            &policy,
        );

        assert_eq!(state.goal.chars().count(), GOAL_MAX_CHARS);
        // GuardedWrite → no constraints.
        assert!(state.active_constraints.is_empty());
        assert_eq!(state.user_intent, "stream");
    }

    // -----------------------------------------------------------------------
    // Test 4: multibyte characters — truncation stays at char boundary
    // -----------------------------------------------------------------------
    #[test]
    fn test_multibyte_truncation_safe() {
        // Each '中' is 3 bytes in UTF-8.  Build a 600-char string.
        let msg: String = "中".repeat(600);
        let truncated = truncate_to_chars(&msg, GOAL_MAX_CHARS);

        // Must be exactly GOAL_MAX_CHARS characters (not bytes).
        assert_eq!(truncated.chars().count(), GOAL_MAX_CHARS);
        // Must be valid UTF-8 (Rust string slicing would panic otherwise, but
        // let's assert explicitly for clarity).
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }
}
