#![cfg(test)]

use super::*;

#[test]
fn whatsapp_signature_invalid_wrong_secret() {
    let app_secret = generate_test_secret();
    let wrong_secret = generate_test_secret();
    let body = b"test body content";

    let signature_header = compute_whatsapp_signature_header(&wrong_secret, body);

    assert!(!verify_whatsapp_signature(&app_secret, body, &signature_header));
}

#[test]
fn whatsapp_signature_invalid_wrong_body() {
    let app_secret = generate_test_secret();
    let original_body = b"original body";
    let tampered_body = b"tampered body";

    let signature_header = compute_whatsapp_signature_header(&app_secret, original_body);

    // Verify with tampered body should fail
    assert!(!verify_whatsapp_signature(
        &app_secret,
        tampered_body,
        &signature_header
    ));
}

#[test]
fn whatsapp_signature_missing_prefix() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    // Signature without "sha256=" prefix
    let signature_header = "abc123def456";

    assert!(!verify_whatsapp_signature(&app_secret, body, signature_header));
}

#[test]
fn whatsapp_signature_empty_header() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    assert!(!verify_whatsapp_signature(&app_secret, body, ""));
}

#[test]
fn whatsapp_signature_invalid_hex() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    // Invalid hex characters
    let signature_header = "sha256=not_valid_hex_zzz";

    assert!(!verify_whatsapp_signature(&app_secret, body, signature_header));
}

#[test]
fn whatsapp_signature_empty_body() {
    let app_secret = generate_test_secret();
    let body = b"";

    let signature_header = compute_whatsapp_signature_header(&app_secret, body);

    assert!(verify_whatsapp_signature(&app_secret, body, &signature_header));
}

#[test]
fn whatsapp_signature_unicode_body() {
    let app_secret = generate_test_secret();
    let body = "Hello 🦀 World".as_bytes();

    let signature_header = compute_whatsapp_signature_header(&app_secret, body);

    assert!(verify_whatsapp_signature(&app_secret, body, &signature_header));
}

#[test]
fn whatsapp_signature_json_payload() {
    let app_secret = generate_test_secret();
    let body = br#"{"entry":[{"changes":[{"value":{"messages":[{"from":"1234567890","text":{"body":"Hello"}}]}}]}]}"#;

    let signature_header = compute_whatsapp_signature_header(&app_secret, body);

    assert!(verify_whatsapp_signature(&app_secret, body, &signature_header));
}

#[test]
fn whatsapp_signature_case_sensitive_prefix() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    let hex_sig = compute_whatsapp_signature_hex(&app_secret, body);

    // Wrong case prefix should fail
    let wrong_prefix = format!("SHA256={hex_sig}");
    assert!(!verify_whatsapp_signature(&app_secret, body, &wrong_prefix));

    // Correct prefix should pass
    let correct_prefix = format!("sha256={hex_sig}");
    assert!(verify_whatsapp_signature(&app_secret, body, &correct_prefix));
}

#[test]
fn whatsapp_signature_truncated_hex() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    let hex_sig = compute_whatsapp_signature_hex(&app_secret, body);
    let truncated = &hex_sig[..32]; // Only half the signature
    let signature_header = format!("sha256={truncated}");

    assert!(!verify_whatsapp_signature(&app_secret, body, &signature_header));
}

#[test]
fn whatsapp_signature_extra_bytes() {
    let app_secret = generate_test_secret();
    let body = b"test body";

    let hex_sig = compute_whatsapp_signature_hex(&app_secret, body);
    let extended = format!("{hex_sig}deadbeef");
    let signature_header = format!("sha256={extended}");

    assert!(!verify_whatsapp_signature(&app_secret, body, &signature_header));
}
