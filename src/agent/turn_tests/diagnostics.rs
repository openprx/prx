#![cfg(test)]
#![allow(
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    clippy::needless_collect,
    clippy::unreadable_literal
)]

use super::*;

#[test]
fn test_scrub_credentials() {
    let input = "API_KEY=sk-1234567890abcdef; token: 1234567890; password=\"secret123456\"";
    let scrubbed = scrub_credentials(input);
    assert!(scrubbed.contains("API_KEY=sk-1*[REDACTED]"));
    assert!(scrubbed.contains("token: 1234*[REDACTED]"));
    assert!(scrubbed.contains("password=\"secr*[REDACTED]\""));
    assert!(!scrubbed.contains("abcdef"));
    assert!(!scrubbed.contains("secret123456"));
}

#[test]
fn test_scrub_credentials_json() {
    let input = r#"{"api_key": "sk-1234567890", "other": "public"}"#;
    let scrubbed = scrub_credentials(input);
    assert!(scrubbed.contains("\"api_key\": \"sk-1*[REDACTED]\""));
    assert!(scrubbed.contains("public"));
}

#[test]
fn sanitize_tool_parse_log_preview_redacts_and_truncates() {
    let input = format!(
        "Bearer abcdefghijklmnopqrstuvwxyz sk-1234567890abcdef key=ABCDEF0123456789 {}",
        "x".repeat(260)
    );

    let preview = sanitize_tool_parse_log_preview(&input);
    assert!(preview.chars().count() <= TOOL_PARSE_LOG_PREVIEW_CHARS);
    assert!(!preview.contains("abcdefghijklmnopqrstuvwxyz"));
    assert!(!preview.contains("1234567890abcdef"));
    assert!(!preview.contains("ABCDEF0123456789"));
    assert!(preview.contains("Bearer [REDACTED]"));
    assert!(preview.contains("key=[REDACTED]"));
}

#[test]
fn scrub_credentials_empty_input() {
    let result = scrub_credentials("");
    assert_eq!(result, "");
}

#[test]
fn scrub_credentials_no_sensitive_data() {
    let input = "normal text without any secrets";
    let result = scrub_credentials(input);
    assert_eq!(result, input, "non-sensitive text should pass through unchanged");
}

#[test]
fn scrub_credentials_short_keyed_values_are_redacted_without_a_prefix() {
    // D-054 amendment 2: a credential-keyed value is redacted at any length; a
    // short one is shown without a prefix, which would give most of it away.
    let result = scrub_credentials(r#"api_key="short"; token=y"#);
    assert_eq!(result, r#"api_key="*[REDACTED]"; token=*[REDACTED]"#);
}

/// R-C2BUg G-1: the model-output parse preview scrubs every credentialed
/// URL, including text that starts with one.
#[test]
fn the_tool_parse_preview_redacts_every_url_in_url_first_text() {
    crate::providers::label_redaction_tests::assert_every_url_redacted("tool parse preview", &|text| {
        sanitize_tool_parse_log_preview(text)
    });
}

/// REDACT-2 (D-054 amendment): tool-output previews detect secrets with the one
/// redactor and only choose how to show them (a short prefix), so an AWS key id
/// or a token with no `key=` in front is scrubbed too. Fails before REDACT-2.
#[test]
fn scrub_credentials_detects_with_the_one_redactor() {
    let scrubbed = scrub_credentials(
        "export AWS=AKIAIOSFODNN7EXAMPLE; gh auth with ghp_abcdefghijklmnop0123; https://u:pw12345678@h.example/x",
    );
    assert!(!scrubbed.contains("AKIAIOSFODNN7EXAMPLE"), "{scrubbed}");
    assert!(!scrubbed.contains("ghp_abcdefghijklmnop0123"), "{scrubbed}");
    assert!(!scrubbed.contains("pw12345678"), "{scrubbed}");
    assert!(
        scrubbed.contains("AKIA*[REDACTED]"),
        "the preview keeps a short prefix: {scrubbed}"
    );
}

/// REDACT-2: logged tool arguments use the one redactor's credential keys and
/// redact secrets inside other string values. Fails before REDACT-2.
#[test]
fn logged_tool_arguments_are_redacted_by_key_and_by_content() {
    let redacted = redact_sensitive_json_keys(&serde_json::json!({
        "command": "curl -H 'Authorization: Bearer abcdefghijklmnop' https://api.example --token=y",
        "api_key": "short",
        "authToken": "t",
        "tokenizer": "cl100k",
        "count": 2
    }));
    let text = redacted.to_string();
    assert!(!text.contains("abcdefghijklmnop"), "{text}");
    assert!(!text.contains("--token=y"), "{text}");
    assert_eq!(
        redacted.get("api_key").and_then(serde_json::Value::as_str),
        Some("[REDACTED]")
    );
    assert_eq!(
        redacted.get("authToken").and_then(serde_json::Value::as_str),
        Some("[REDACTED]")
    );
    assert_eq!(
        redacted.get("tokenizer").and_then(serde_json::Value::as_str),
        Some("cl100k"),
        "a tokenizer is not a credential"
    );
    assert_eq!(redacted.get("count").and_then(serde_json::Value::as_u64), Some(2));
}

/// R-C2BUk K-5: only the preview is kept, so only a bounded prefix is
/// redacted. The redactor's work does not grow with the model's response
/// (it redacted the whole text, then cut 200 characters), and a secret that
/// starts inside the preview is still redacted whole.
#[test]
fn a_tool_parse_preview_redacts_a_bounded_prefix() {
    let one = "sk-a ".repeat(1024 * 1024 / 5);
    let two = "sk-a ".repeat(2 * 1024 * 1024 / 5);
    let work_one = crate::security::redaction::redaction_work(|| sanitize_tool_parse_log_preview(&one));
    let work_two = crate::security::redaction::redaction_work(|| sanitize_tool_parse_log_preview(&two));
    assert_eq!(
        work_one,
        work_two,
        "the preview's redaction must not grow with the input ({} vs {} bytes)",
        one.len(),
        two.len()
    );
    assert!(work_one < one.len(), "{work_one} bytes examined for a 1 MB input");

    let secret = "A1b2".repeat(20);
    let text = format!("{} sk-{secret} {}", "x".repeat(150), "y".repeat(40_000));
    let preview = sanitize_tool_parse_log_preview(&text);
    assert!(!preview.contains("A1b2A1b2"), "{preview}");
    assert!(preview.chars().count() <= TOOL_PARSE_LOG_PREVIEW_CHARS);
}

/// D-069 L-4: a PEM private-key block whose END lies beyond the redacted
/// prefix is redacted from its BEGIN to the cut. The redactor needs the END,
/// so the key body used to reach the preview. A public key is not cut.
#[test]
fn a_private_key_without_its_end_in_the_preview_prefix_is_redacted() {
    let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC".repeat(1_000);
    let text = format!("tool output: -----BEGIN RSA PRIVATE KEY-----\n{body}\n-----END RSA PRIVATE KEY-----");
    assert!(text.len() > 16 * 1024, "test: the END lies beyond the redacted prefix");
    let preview = sanitize_tool_parse_log_preview(&text);
    assert!(!preview.contains("MIIEvQIBADAN"), "{preview}");
    assert!(preview.starts_with("tool output: "), "{preview}");
    assert!(preview.ends_with(crate::security::redaction::REDACTED), "{preview}");

    let public = format!("-----BEGIN PUBLIC KEY-----\n{}", "QUJD".repeat(100));
    assert!(sanitize_tool_parse_log_preview(&public).contains("QUJD"));
}
