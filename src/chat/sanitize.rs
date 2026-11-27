//! Privacy sanitization and output truncation for session persistence.
//!
//! Applied before storing chat sessions to avoid persisting secrets
//! or excessively large tool outputs. Secrets are redacted by the one
//! redactor, [`crate::security::redaction`]; this module adds chat's caps.

use crate::security::redaction::redact_secrets;

/// Maximum tool output size (bytes) before truncation.
const MAX_TOOL_OUTPUT_BYTES: usize = 10 * 1024; // 10KB

/// Truncate content if it exceeds the maximum size, appending a hash reference.
pub fn truncate_large_output(content: &str) -> String {
    truncate_large_output_with_total(content, content.len())
}

fn truncate_large_output_with_total(content: &str, original_len: usize) -> String {
    if content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return content.to_string();
    }
    // Use a simple hash for reference
    let hash = {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(content.as_bytes());
        let result = hasher.finalize();
        hex::encode(result.get(..8).unwrap_or(result.as_slice()))
    };
    // Floor to a valid UTF-8 char boundary to avoid panic on multi-byte chars
    let marker = format!("\n\n[... truncated ({original_len} bytes total, ref: {hash})]");
    let mut end = MAX_TOOL_OUTPUT_BYTES.saturating_sub(marker.len());
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = &content[..end];
    format!("{truncated}{marker}")
}

/// Apply both sanitization steps: redact secrets and truncate.
pub fn sanitize_for_persistence(content: &str) -> String {
    let redacted = redact_secrets(content);
    truncate_large_output_with_total(&redacted, redacted.len())
}

pub fn sanitize_text_or_json(content: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(mut value @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => {
            sanitize_json_value(&mut value);
            serde_json::to_string(&value).unwrap_or_else(|_| sanitize_for_persistence(content))
        }
        _ => sanitize_for_persistence(content),
    }
}

/// Tool-call arguments as raw text, redacted before any preview is built from
/// them: a JSON object or array is redacted by value (decoded strings, so a
/// quote escaped once inside a string is seen as the redactor reads it); other
/// text is redacted as a whole. Never truncates. A preview built from the
/// unredacted text re-escapes the quotes of a shell command and hides a
/// credential pair from the text redactor.
pub fn redact_tool_args_text(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(mut value @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => {
            crate::security::redaction::redact_json_value(&mut value);
            serde_json::to_string(&value).unwrap_or_else(|_| redact_secrets(raw))
        }
        _ => redact_secrets(raw),
    }
}

/// Redact every JSON string and credential-named value (the one redactor),
/// then cap each string like stored session content.
pub fn sanitize_json_value(value: &mut serde_json::Value) {
    crate::security::redaction::redact_json_value(value);
    cap_json_strings(value);
}

fn cap_json_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => object.values_mut().for_each(cap_json_strings),
        serde_json::Value::Array(items) => items.iter_mut().for_each(cap_json_strings),
        serde_json::Value::String(text) => *text = truncate_large_output(text),
        _ => {}
    }
}

pub fn sanitize_json_structure<T>(value: &T) -> anyhow::Result<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let mut json = serde_json::to_value(value)?;
    sanitize_json_value(&mut json);
    Ok(serde_json::from_value(json)?)
}

/// One stored turn's content.
///
/// A context handoff note is runtime metadata (ids, row ranges, counts) in a
/// fixed JSON schema; text redaction matches its key names
/// (`source_token_estimate`) and would leave an unparseable note that a
/// resumed session or a branch copy can no longer follow. Only a validated
/// handoff takes the exception: the turn carries compaction provenance
/// ([`super::session::ChatTurn::context_handoff`], never set for a model
/// reply) and its note passes
/// [`crate::agent::compaction::validated_context_handoff_note`] (exact schema,
/// no field outside the note's metadata, bounded size). Its string values are
/// redacted one by one; an unchanged note keeps its exact bytes (the
/// transcript row carries the same text, and later compaction provenance
/// matches on it) and is never truncated. Every other turn, including any
/// note with an extra field such as `"password"`, takes the whole-text
/// redaction (credential keys included) and the size cap.
fn sanitize_turn_content(turn: &super::session::ChatTurn) -> String {
    if turn.context_handoff
        && turn.role == "assistant"
        && let Some(note) = crate::agent::compaction::validated_context_handoff_note(&turn.content)
    {
        let mut redacted = note.clone();
        redact_string_values(&mut redacted);
        if redacted == note {
            return turn.content.clone();
        }
        if let Ok(json) = serde_json::to_string(&redacted) {
            return crate::agent::compaction::format_context_handoff_message(&json);
        }
    }
    sanitize_for_persistence(&turn.content)
}

fn redact_string_values(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => object.values_mut().for_each(redact_string_values),
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_string_values),
        serde_json::Value::String(text) => *text = redact_secrets(text),
        _ => {}
    }
}

/// Apply the authoritative persistence/export content policy to a session
/// clone while preserving its schema and non-content metadata.
pub fn sanitize_session_content(session: &super::session::ChatSession) -> super::session::ChatSession {
    let mut sanitized = session.clone();
    sanitized.title = sanitize_for_persistence(&sanitized.title);
    for turn in &mut sanitized.turns {
        turn.content = sanitize_turn_content(turn);
        // What was put around a request is stored masked, like its text.
        if let Some(sent) = turn.tool_evidence.as_mut().and_then(|evidence| evidence.sent.as_mut()) {
            sent.prefix = redact_secrets(&sent.prefix);
            sent.tail = redact_secrets(&sent.tail);
        }
        for tool_call in &mut turn.tool_calls {
            tool_call.args_preview = sanitize_text_or_json(&tool_call.args_preview);
        }
    }
    for background in &mut sanitized.background_sessions {
        background.title = sanitize_for_persistence(&background.title);
        background.summary = sanitize_for_persistence(&background.summary);
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A note in the exact shape the compaction code writes, referencing
    /// `event_count` UUID events.
    fn handoff_note(event_count: usize) -> serde_json::Value {
        let event_ids: Vec<String> = (0..event_count)
            .map(|index| format!("00000000-0000-4000-8000-{index:012}"))
            .collect();
        let range = serde_json::json!({
            "first_event_id": event_ids.first().cloned().unwrap_or_default(),
            "last_event_id": event_ids.last().cloned().unwrap_or_default(),
            "first_row_id": 1,
            "last_row_id": event_count,
            "source_event_count": event_count,
        });
        serde_json::json!({
            "schema_version": 1,
            "kind": "context_handoff",
            "strategy": "hard_switch",
            "handoff_event_id": "11111111-1111-4111-8111-111111111111",
            "parent_handoff_event_id": null,
            "from_generation": 0,
            "to_generation": 1,
            "created_at": "2026-11-16T00:00:00+00:00",
            "trigger": "chat_manual_compact",
            "source_message_count": event_count,
            "source_token_estimate": 588,
            "unpersisted_companion_count": 0,
            "folded_tool_roundtrips": {"count": 0, "tools": []},
            "folded_tool_roundtrips_notice": "no tool round-trips were folded into this handoff",
            "provenance_status": "exact",
            "source_event_range": range,
            "retained_recent_message_count": 2,
            "lookup": {
                "tool": "transcript_history_lookup",
                "arguments": {"event_ids": event_ids, "first_row_id": 1, "last_row_id": event_count}
            }
        })
    }

    fn handoff_turn(note: &serde_json::Value, context_handoff: bool) -> super::super::session::ChatTurn {
        super::super::session::ChatTurn {
            role: "assistant".to_owned(),
            content: crate::agent::compaction::format_context_handoff_message(
                &serde_json::to_string(note).expect("test: encode note"),
            ),
            timestamp: chrono::Utc::now(),
            tool_calls: Vec::new(),
            context_handoff,
            tool_evidence: None,
            event_id: None,
        }
    }

    #[test]
    fn a_validated_handoff_turn_keeps_its_note_parseable_and_byte_exact() {
        let turn = handoff_turn(&handoff_note(3), true);

        assert_eq!(
            sanitize_turn_content(&turn),
            turn.content,
            "a validated handoff note is stored byte for byte"
        );
        let mut user = turn.clone();
        user.role = "user".to_owned();
        assert_ne!(
            sanitize_turn_content(&user),
            turn.content,
            "only an assistant handoff turn takes the note path"
        );
    }

    #[test]
    fn a_note_without_compaction_provenance_takes_the_whole_text_redaction() {
        let model_written = handoff_turn(&handoff_note(3), false);

        let stored = sanitize_turn_content(&model_written);

        assert_ne!(
            stored, model_written.content,
            "a model reply shaped like a handoff is redacted as text"
        );
        assert!(
            crate::agent::compaction::validated_context_handoff_note(&stored).is_none(),
            "a note without provenance never keeps the exception"
        );
    }

    #[test]
    fn a_credential_field_in_a_handoff_note_is_redacted() {
        let mut note = handoff_note(3);
        note.as_object_mut()
            .expect("test: note object")
            .insert("password".to_owned(), serde_json::json!("sample-value"));
        let turn = handoff_turn(&note, true);

        let stored = sanitize_turn_content(&turn);

        assert!(
            !stored.contains("sample-value"),
            "a field outside the note's metadata keeps the credential-key redaction: {stored}"
        );
        assert!(
            crate::agent::compaction::validated_context_handoff_note(&turn.content).is_none(),
            "an extra field makes the note invalid"
        );
    }

    #[test]
    fn a_secret_inside_a_validated_handoff_value_is_still_redacted() {
        let mut note = handoff_note(3);
        note.as_object_mut().expect("test: note object").insert(
            "trigger".to_owned(),
            serde_json::json!("sk-abc123456789012345678901234567890123456789"),
        );
        let turn = handoff_turn(&note, true);
        assert!(
            crate::agent::compaction::validated_context_handoff_note(&turn.content).is_some(),
            "test: the identifier-shaped secret keeps the note valid"
        );

        let stored = sanitize_turn_content(&turn);

        assert!(!stored.contains("sk-abc"), "the secret value is redacted: {stored}");
        let parsed = crate::agent::compaction::parse_context_handoff_note(&stored)
            .expect("test: the redacted note stays parseable");
        assert_eq!(
            parsed.get("source_token_estimate"),
            Some(&serde_json::json!(588)),
            "the note's metadata survives the per-value redaction"
        );
    }

    #[test]
    fn the_largest_supported_handoff_is_stored_whole_and_a_larger_one_is_not_validated() {
        let largest = handoff_turn(
            &handoff_note(crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS),
            true,
        );
        assert!(
            largest.content.len() > MAX_TOOL_OUTPUT_BYTES,
            "test: the largest note exceeds the generic text cap"
        );

        let stored = sanitize_turn_content(&largest);

        assert_eq!(stored, largest.content, "the largest supported note is never truncated");
        assert!(
            crate::agent::compaction::validated_context_handoff_note(&stored).is_some(),
            "the stored note still validates"
        );
        let over = handoff_turn(
            &handoff_note(crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS + 1),
            true,
        );
        assert!(
            crate::agent::compaction::validated_context_handoff_note(&over.content).is_none(),
            "a note over the supported range is not a validated handoff"
        );
    }

    #[test]
    fn redacts_api_keys() {
        let input = "api_key: sk-abc123456789012345678901234567890123456789";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("sk-abc"));
    }

    #[test]
    fn redacts_openai_keys() {
        let input = "Using key sk-proj1234567890abcdefghijklmnop";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn redacts_github_pat() {
        // GitHub PAT regex requires exactly 36 alphanumeric chars after ghp_
        let input = "token=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let result = redact_secrets(input);
        assert!(result.contains("[REDACTED]"));
    }

    #[test]
    fn preserves_normal_text() {
        let input = "This is a normal message about programming";
        let result = redact_secrets(input);
        assert_eq!(result, input);
    }

    #[test]
    fn truncates_large_output() {
        let large = "x".repeat(20_000);
        let result = truncate_large_output(&large);
        assert!(result.len() < large.len());
        assert!(result.contains("[... truncated"));
        assert!(result.contains("20000 bytes total"));
    }

    #[test]
    fn does_not_truncate_small_output() {
        let small = "small output";
        let result = truncate_large_output(small);
        assert_eq!(result, small);
    }

    #[test]
    fn sanitize_combined() {
        let input = "password: hunter2\nResult output here";
        let result = sanitize_for_persistence(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("hunter2"));
    }

    #[test]
    fn truncate_respects_utf8_char_boundaries() {
        // Create a string of 3-byte characters where MAX_TOOL_OUTPUT_BYTES
        // would land mid-character
        let wide = "\u{20ac}".repeat(5000); // each '\u{20ac}' = 3 bytes -> 15000 bytes
        let result = truncate_large_output(&wide);
        assert!(result.contains("[... truncated"));
        // The truncated portion must be valid UTF-8 (no panic from slicing)
        assert!(result.is_char_boundary(0)); // trivially true, but validates result is valid
    }

    #[test]
    fn sanitization_is_bounded_and_idempotent() {
        let input = format!("Authorization: Bearer token-secret\n{}", "\u{20ac}".repeat(5_000));
        let once = sanitize_for_persistence(&input);
        let twice = sanitize_for_persistence(&once);
        assert!(once.len() <= MAX_TOOL_OUTPUT_BYTES);
        assert_eq!(once, twice);
        assert_eq!(once.matches("[... truncated (").count(), 1);
        assert_eq!(once.matches("bytes total, ref:").count(), 1);
        assert!(once.contains(&format!("{} bytes total", redact_secrets(&input).len())));
    }

    #[test]
    fn redacts_plain_and_recursive_json_secret_forms() {
        let authorization = sanitize_for_persistence("Authorization: Bearer abcdefghijklmnop");
        assert!(!authorization.contains("abcdefghijklmnop"));
        let password_json = sanitize_for_persistence(r#"{"password":"tiny"}"#);
        assert!(!password_json.contains("tiny"));
        let project_key = sanitize_for_persistence("sk-proj-abcdefghijklmnopqrstuv");
        assert!(!project_key.contains("abcdefghijklmnopqrstuv"));
        let mut value = serde_json::json!({
            "detail": ["Authorization: Bearer abcdefghijklmnop", {"password": "tiny"}],
            "nested": {"error": "AKIAABCDEFGHIJKLMNOP"}
        });
        sanitize_json_value(&mut value);
        let encoded = serde_json::to_string(&value).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&encoded).is_ok());
        assert!(!encoded.contains("abcdefghijklmnop"));
        assert!(!encoded.contains("tiny"));
        assert!(!encoded.contains("AKIAABCDEFGHIJKLMNOP"));
    }
}
