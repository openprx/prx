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

/// Apply the authoritative persistence/export content policy to a session
/// clone while preserving its schema and non-content metadata.
pub fn sanitize_session_content(session: &super::session::ChatSession) -> super::session::ChatSession {
    let mut sanitized = session.clone();
    sanitized.title = sanitize_for_persistence(&sanitized.title);
    for turn in &mut sanitized.turns {
        turn.content = sanitize_for_persistence(&turn.content);
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
