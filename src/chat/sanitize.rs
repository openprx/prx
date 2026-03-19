//! Privacy sanitization and output truncation for session persistence.
//!
//! Applied before storing chat sessions to avoid persisting secrets
//! or excessively large tool outputs.

use regex::Regex;
use std::sync::LazyLock;

/// Maximum tool output size (bytes) before truncation.
const MAX_TOOL_OUTPUT_BYTES: usize = 10 * 1024; // 10KB

/// Patterns that look like secrets (API keys, tokens, passwords).
static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    // These patterns are intentionally broad — false positives are acceptable
    // because redaction in persisted sessions is a safety measure.
    [
        r"(?i)(api[_-]?key|api[_-]?secret|auth[_-]?token|access[_-]?token|bearer)\s*[:=]\s*\S{8,}",
        r"(?i)(password|passwd|pwd)\s*[:=]\s*\S{4,}",
        r"sk-[a-zA-Z0-9]{20,}",          // OpenAI-style keys
        r"sk-ant-[a-zA-Z0-9-]{20,}",     // Anthropic keys
        r"ghp_[a-zA-Z0-9]{36}",           // GitHub PAT
        r"gho_[a-zA-Z0-9]{36}",           // GitHub OAuth
        r"glpat-[a-zA-Z0-9_-]{20,}",     // GitLab PAT
        r"xoxb-[a-zA-Z0-9-]+",            // Slack bot tokens
        r"xoxp-[a-zA-Z0-9-]+",            // Slack user tokens
        r"AKIA[0-9A-Z]{16}",              // AWS access key IDs
    ]
    .iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
});

/// Redact known secret patterns in text.
pub fn redact_secrets(text: &str) -> String {
    let mut result = text.to_string();
    for pattern in SECRET_PATTERNS.iter() {
        result = pattern.replace_all(&result, "[REDACTED]").to_string();
    }
    result
}

/// Truncate content if it exceeds the maximum size, appending a hash reference.
pub fn truncate_large_output(content: &str) -> String {
    if content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return content.to_string();
    }
    // Use a simple hash for reference
    let hash = {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(content.as_bytes());
        let result = hasher.finalize();
        hex::encode(&result[..8])
    };
    // Floor to a valid UTF-8 char boundary to avoid panic on multi-byte chars
    let mut end = MAX_TOOL_OUTPUT_BYTES;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = &content[..end];
    format!(
        "{truncated}\n\n[... truncated ({} bytes total, ref: {hash})]",
        content.len()
    )
}

/// Apply both sanitization steps: redact secrets and truncate.
pub fn sanitize_for_persistence(content: &str) -> String {
    let redacted = redact_secrets(content);
    truncate_large_output(&redacted)
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
        // Create a string of multi-byte CJK chars where MAX_TOOL_OUTPUT_BYTES
        // would land mid-character
        let cjk = "你".repeat(5000); // each '你' = 3 bytes → 15000 bytes
        let result = truncate_large_output(&cjk);
        assert!(result.contains("[... truncated"));
        // The truncated portion must be valid UTF-8 (no panic from slicing)
        assert!(result.is_char_boundary(0)); // trivially true, but validates result is valid
    }
}
