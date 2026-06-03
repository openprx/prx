//! Build a [`CausalState`] snapshot from agent-loop runtime data.
//!
//! This module contains a single pure function,
//! [`build_causal_state_from_chat`], that maps the agent loop's in-flight
//! request context (a flat `&[ChatMessage]` history) into the immutable
//! [`CausalState`] value consumed by the rest of the Causal Tree Engine.

use super::state::{
    ArtifactRef, ArtifactSource, ArtifactType, BudgetState, CausalState, SideEffectMode, StepRecord, StepStatus,
};
use crate::agent::classifier::{ClassifyResult, TaskIntent};

/// Maximum number of UTF-8 characters kept for the `goal` field.
const GOAL_MAX_CHARS: usize = 512;

/// Prefix the agent loop (`loop_.rs`) uses when it folds tool results back into a
/// synthetic `role == "user"` message in prompt/text mode (the provider does not
/// support native `role == "tool"` messages). Kept in sync with the producer at
/// `crate::agent::loop_` (the `history.push(ChatMessage::user(format!("[Tool
/// results]\n{...}")))` site) — both reference this constant so a wording change
/// stays consistent across producer and parser.
pub(crate) const TOOL_RESULTS_PREFIX: &str = "[Tool results]";

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
const fn intent_label(intent: TaskIntent) -> &'static str {
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
fn tool_result_to_artifact(_tool_call_id: &str, content: &str, index: usize) -> ArtifactRef {
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

/// Parse a native-mode tool history message.
///
/// The agent loop writes native tool results as a JSON object
/// `{"tool_call_id": <id>, "content": <result>}` (see `loop_.rs` ~4479). This
/// extracts the `tool_call_id` and `content` fields so the artifact summary is
/// the tool result body, not the JSON envelope.
///
/// Falls back to `("", Borrowed(raw))` when the message is not the expected JSON
/// object (bare-string tool messages, or any other shape) — this keeps backward
/// compatibility with text-mode providers and existing fixtures.
///
/// The body is returned as a [`Cow`]: `Borrowed` for the bare-string fallback
/// (zero copy), `Owned` for the JSON path (the decoded `content` string, which
/// `serde_json` materialises with escapes resolved — re-borrowing into `raw`
/// would be unsound when the value contained escaped characters).
#[cfg(feature = "llm-router")]
fn parse_native_tool_message(raw: &str) -> (String, std::borrow::Cow<'_, str>) {
    if let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str::<serde_json::Value>(raw) {
        if let Some(serde_json::Value::String(content)) = map.remove("content") {
            let call_id = match map.remove("tool_call_id") {
                Some(serde_json::Value::String(id)) => id,
                _ => String::new(),
            };
            return (call_id, std::borrow::Cow::Owned(content));
        }
    }
    (String::new(), std::borrow::Cow::Borrowed(raw))
}

/// Build an immutable [`CausalState`] snapshot from the **agent-loop** runtime,
/// whose conversation history is a flat `&[ChatMessage]` (role + content).
///
/// Consumed by `loop_::run` (the live agent tool loop) via the CTE branch
/// prediction hook. This is the only `CausalState` constructor: the legacy
/// `&[ConversationMessage]` variant was removed together with the dead CTE wiring
/// on the legacy `Agent` path.
///
/// # Dual-path tool-artifact extraction
///
/// The agent loop writes tool outputs back into history in **two** shapes,
/// depending on whether the provider supports native tool messages:
///
/// - **native mode** — one `role == "tool"` message per tool call;
/// - **prompt/text mode** — a single synthetic `role == "user"` message whose
///   content starts with [`TOOL_RESULTS_PREFIX`] (`"[Tool results]"`).
///
/// Both are mapped to [`ArtifactType::ToolOutput`] artifacts. A plain user input
/// (no prefix) is *not* mapped — only assistant steps and tool results become
/// state. This avoids the prompt-mode blind spot where looking at
/// `role == "tool"` alone would silently drop every tool artifact.
//
// Gated on `llm-router` because its sole consumer is the CTE branch-prediction
// hook in `loop_::run`, which is itself `#[cfg(feature = "llm-router")]`.
#[cfg(feature = "llm-router")]
pub fn build_causal_state_from_chat(
    session_id: &str,
    user_message: &str,
    classify_result: &ClassifyResult,
    history: &[crate::providers::ChatMessage],
    side_effect_mode: SideEffectMode,
    policy: &super::policy::CausalPolicy,
) -> CausalState {
    let request_id = uuid::Uuid::new_v4().to_string();
    let snapshot_ts = chrono::Utc::now().to_rfc3339();

    let goal = truncate_to_chars(user_message, GOAL_MAX_CHARS).to_string();
    let user_intent = intent_label(classify_result.intent).to_string();

    let mut step_index: usize = 0;
    let mut completed_steps: Vec<StepRecord> = Vec::new();
    let mut artifact_index: usize = 0;
    let mut known_artifacts: Vec<ArtifactRef> = Vec::new();

    for msg in history {
        match msg.role.as_str() {
            "assistant" => {
                completed_steps.push(assistant_chat_to_step(&msg.content, step_index, &snapshot_ts));
                step_index += 1;
            }
            // native mode: one role == "tool" message per tool call.
            "tool" => {
                // The agent loop serialises native tool results as a JSON object
                // `{"tool_call_id": ..., "content": ...}` (see loop_.rs ~4479).
                // Parse it and use the `content` field as the artifact body; fall
                // back to the raw message content for bare strings (the legacy /
                // text shape used in some tests and providers).
                let (call_id, body) = parse_native_tool_message(&msg.content);
                known_artifacts.push(tool_result_to_artifact(&call_id, &body, artifact_index));
                artifact_index += 1;
            }
            // prompt/text mode: a synthetic user message folding all tool results
            // into one block prefixed with TOOL_RESULTS_PREFIX. Strip the prefix
            // (plus the following newline, if any) before summarising so the
            // artifact summary is the result body, not the marker.
            "user" if msg.content.starts_with(TOOL_RESULTS_PREFIX) => {
                let body = msg
                    .content
                    .strip_prefix(TOOL_RESULTS_PREFIX)
                    .unwrap_or(&msg.content)
                    .trim_start_matches('\n');
                known_artifacts.push(tool_result_to_artifact("", body, artifact_index));
                artifact_index += 1;
            }
            // system messages and genuine user input are not mapped (matches the
            // Agent path: only assistant steps + tool results become state).
            _ => {}
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
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::policy::CausalPolicy;
    use super::*;
    use crate::agent::classifier::{ClassifyResult, TaskIntent};
    #[cfg(feature = "llm-router")]
    use crate::providers::ChatMessage;

    #[cfg(feature = "llm-router")]
    fn default_classify(intent: TaskIntent) -> ClassifyResult {
        ClassifyResult {
            intent,
            model_hint: None,
            reason: "test".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // truncate_to_chars: multibyte characters — truncation stays at char boundary
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

    // -----------------------------------------------------------------------
    // build_causal_state_from_chat (loop_ runtime, flat ChatMessage history)
    // -----------------------------------------------------------------------

    #[cfg(feature = "llm-router")]
    #[test]
    fn from_chat_empty_history_basic_fields() {
        let policy = CausalPolicy::default();
        let state = build_causal_state_from_chat(
            "sess-loop",
            "What is the capital of France?",
            &default_classify(TaskIntent::Simple),
            &[],
            SideEffectMode::ReadOnly,
            &policy,
        );
        assert_eq!(state.session_id, "sess-loop");
        assert_eq!(state.goal, "What is the capital of France?");
        assert_eq!(state.user_intent, "simple");
        assert!(state.completed_steps.is_empty());
        assert!(state.known_artifacts.is_empty());
        assert_eq!(state.active_constraints, vec!["no_write"]);
        assert_eq!(state.budget.extra_latency_budget_ms, policy.extra_latency_budget_ms);
        assert!(!state.request_id.is_empty());
    }

    #[cfg(feature = "llm-router")]
    #[test]
    fn from_chat_assistant_messages_become_steps() {
        let policy = CausalPolicy::default();
        let history = vec![
            ChatMessage::system("system prompt"),
            ChatMessage::user("Fix the bug"),
            ChatMessage::assistant("Reading the file..."),
            ChatMessage::assistant("Patch applied successfully."),
        ];
        let state = build_causal_state_from_chat(
            "sess-loop",
            "Fix the bug",
            &default_classify(TaskIntent::Delegate),
            &history,
            SideEffectMode::ApprovalRequired,
            &policy,
        );
        // Only assistant messages become steps; system + user input do not.
        assert_eq!(state.completed_steps.len(), 2);
        assert_eq!(state.completed_steps[0].step_id, "step-0");
        assert_eq!(state.completed_steps[0].label, "Reading the file...");
        assert_eq!(state.completed_steps[0].status, StepStatus::Succeeded);
        assert_eq!(state.completed_steps[1].label, "Patch applied successfully.");
        assert!(state.known_artifacts.is_empty());
        assert_eq!(state.active_constraints, vec!["approval_required"]);
        assert_eq!(state.user_intent, "delegate");
    }

    // Bare-string native tool messages (legacy / text-mode providers) fall back
    // to using the raw message content as the artifact summary.
    #[cfg(feature = "llm-router")]
    #[test]
    fn from_chat_native_tool_messages_bare_string_fallback() {
        let policy = CausalPolicy::default();
        let history = vec![
            ChatMessage::user("run a tool"),
            ChatMessage::assistant("calling the tool"),
            ChatMessage::tool("native tool output A"),
            ChatMessage::tool("native tool output B"),
        ];
        let state = build_causal_state_from_chat(
            "sess-loop",
            "run a tool",
            &default_classify(TaskIntent::Stream),
            &history,
            SideEffectMode::GuardedWrite,
            &policy,
        );
        assert_eq!(state.completed_steps.len(), 1);
        assert_eq!(state.known_artifacts.len(), 2);
        assert_eq!(state.known_artifacts[0].artifact_id, "artifact-0");
        assert_eq!(state.known_artifacts[0].artifact_type, ArtifactType::ToolOutput);
        assert_eq!(state.known_artifacts[0].source, ArtifactSource::ToolExecution);
        assert_eq!(state.known_artifacts[0].summary, "native tool output A");
        assert_eq!(state.known_artifacts[1].summary, "native tool output B");
        // GuardedWrite → no constraints.
        assert!(state.active_constraints.is_empty());
    }

    // Production native tool history is JSON `{"tool_call_id", "content"}` (see
    // loop_.rs ~4479). The artifact summary must be the decoded `content` body,
    // not the JSON envelope.
    #[cfg(feature = "llm-router")]
    #[test]
    fn from_chat_native_tool_messages_json_extracts_content() {
        let policy = CausalPolicy::default();
        // Build the exact shape the agent loop writes, including an escaped
        // newline inside `content` to exercise JSON decoding.
        let tool_a = serde_json::json!({
            "tool_call_id": "call_abc",
            "content": "line one\nline two",
        })
        .to_string();
        let tool_b = serde_json::json!({
            "tool_call_id": "call_def",
            "content": "result B",
        })
        .to_string();
        let history = vec![
            ChatMessage::user("run a tool"),
            ChatMessage::assistant("calling the tool"),
            ChatMessage::tool(tool_a),
            ChatMessage::tool(tool_b),
        ];
        let state = build_causal_state_from_chat(
            "sess-loop",
            "run a tool",
            &default_classify(TaskIntent::Stream),
            &history,
            SideEffectMode::GuardedWrite,
            &policy,
        );
        assert_eq!(state.known_artifacts.len(), 2);
        // Summary is the decoded `content`, NOT the JSON envelope.
        assert_eq!(state.known_artifacts[0].summary, "line one\nline two");
        assert_eq!(state.known_artifacts[1].summary, "result B");
        assert!(!state.known_artifacts[0].summary.contains("tool_call_id"));
    }

    // parse_native_tool_message: JSON object → decoded content; anything else →
    // raw fallback.
    #[cfg(feature = "llm-router")]
    #[test]
    fn parse_native_tool_message_json_and_fallback() {
        let json = serde_json::json!({"tool_call_id": "id1", "content": "hello\tworld"}).to_string();
        let (id, body) = parse_native_tool_message(&json);
        assert_eq!(id, "id1");
        assert_eq!(body.as_ref(), "hello\tworld");

        // Bare string → empty id, borrowed raw body.
        let (id, body) = parse_native_tool_message("plain text");
        assert_eq!(id, "");
        assert_eq!(body.as_ref(), "plain text");

        // JSON without a string `content` field → fallback to raw.
        let no_content = r#"{"tool_call_id":"x","content":123}"#;
        let (_, body) = parse_native_tool_message(no_content);
        assert_eq!(body.as_ref(), no_content);
    }

    #[cfg(feature = "llm-router")]
    #[test]
    fn from_chat_prompt_mode_tool_results_user_becomes_artifact() {
        let policy = CausalPolicy::default();
        // prompt/text mode: tool results folded into a synthetic user message.
        let folded = format!("{TOOL_RESULTS_PREFIX}\nfile content here\nanother line");
        let history = vec![
            ChatMessage::user("Fix the bug"),
            ChatMessage::assistant("Reading the file..."),
            ChatMessage::user(folded),
        ];
        let state = build_causal_state_from_chat(
            "sess-loop",
            "Fix the bug",
            &default_classify(TaskIntent::Simple),
            &history,
            SideEffectMode::ReadOnly,
            &policy,
        );
        // One assistant step; the folded "[Tool results]" user message → one artifact.
        assert_eq!(state.completed_steps.len(), 1);
        assert_eq!(state.known_artifacts.len(), 1);
        assert_eq!(state.known_artifacts[0].artifact_type, ArtifactType::ToolOutput);
        // Prefix (and the following newline) stripped from the summary.
        assert_eq!(state.known_artifacts[0].summary, "file content here\nanother line");
    }

    #[cfg(feature = "llm-router")]
    #[test]
    fn from_chat_plain_user_input_is_not_an_artifact() {
        let policy = CausalPolicy::default();
        // A genuine user message that merely mentions the words must not be folded
        // unless it actually *starts* with the prefix.
        let history = vec![ChatMessage::user("please show me the [Tool results] format")];
        let state = build_causal_state_from_chat(
            "sess-loop",
            "question",
            &default_classify(TaskIntent::Simple),
            &history,
            SideEffectMode::ReadOnly,
            &policy,
        );
        assert!(state.known_artifacts.is_empty());
        assert!(state.completed_steps.is_empty());
    }
}
