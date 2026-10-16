#![cfg(test)]

use super::*;

/// Pairing adds a token; it does not get to edit the operator's file.
///
/// `gateway.paired_tokens` is a restart-only field, so the running guard's
/// set is not authoritative over what is on disk: an entry added by hand
/// while the gateway is up is invisible to the guard, and persisting the
/// guard's set wholesale deleted it. The same assignment also rewrote a
/// plaintext entry into its hash — the same credential, but not the same
/// text, and chat's `/sessions --daemon` fallback only accepts the
/// plaintext form, so an entry that was doing a job silently stopped doing
/// it with nothing in the file to show why.
///
/// Both entries here are ones pairing has never seen. Neither may move.
#[tokio::test]
async fn a_configured_token_survives_a_later_pairing() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("config.toml");

    let mut config = Config::default();
    config.config_path = config_path.clone();
    config.workspace_dir = temp.path().join("workspace");
    config.gateway.paired_tokens = vec!["zc_written_by_the_operator".to_string(), "a".repeat(64)];

    // Empty at startup, so a code exists and a pairing can complete — the
    // entries above landed in the file afterwards.
    let guard = PairingGuard::new(true, &[]);
    let code = guard.pairing_code().unwrap();
    let minted = guard.try_pair(&code, "test_client").await.unwrap().unwrap();

    let shared = crate::config::new_shared(config);
    persist_pairing_tokens(&shared, &guard).await.unwrap();

    // `paired_tokens` is encrypted at rest (SECRETS-OTHER): on disk every
    // entry is `enc2:`, and the loader restores each spelling.
    let saved = tokio::fs::read_to_string(&config_path).await.unwrap();
    assert!(
        !saved.contains("zc_written_by_the_operator"),
        "paired tokens are encrypted at rest"
    );
    let parsed = Config::load_from_path(&config_path, temp.path().join("workspace")).unwrap();
    assert_eq!(
        parsed.gateway.paired_tokens.first().map(String::as_str),
        Some("zc_written_by_the_operator"),
        "the operator's plaintext entry must still be there, still plaintext: {:?}",
        parsed.gateway.paired_tokens
    );
    assert_eq!(
        parsed.gateway.paired_tokens.get(1).map(String::as_str),
        Some("a".repeat(64).as_str()),
        "an entry pairing never loaded must not be deleted by pairing: {:?}",
        parsed.gateway.paired_tokens
    );
    // And the newly minted token was still persisted — as a hash, never as
    // the plaintext handed to the client.
    assert_eq!(parsed.gateway.paired_tokens.len(), 3);
    let appended = &parsed.gateway.paired_tokens[2];
    assert!(
        crate::security::pairing::is_token_hash(appended),
        "the minted token must be persisted hashed, got {appended}"
    );
    assert_ne!(appended.as_str(), minted.as_str());
    assert!(
        !saved.contains(minted.as_str()),
        "a generated token must never reach the file in plaintext"
    );
}

/// A configured plaintext entry and the hash pairing holds for it are one
/// credential, and the file keeps the operator's spelling of it.
///
/// Without the dedupe the file would grow a second entry for the same token
/// on every pairing; with the dedupe pointed the other way the plaintext
/// would lose to the hash, which is the rewrite this exists to prevent.
#[test]
fn a_plaintext_entry_absorbs_the_hash_pairing_holds_for_it() {
    let plaintext = "zc_written_by_the_operator";
    let hash = crate::security::pairing::hash_token(plaintext);

    let merged = merge_paired_tokens(
        &[format!("  {plaintext}  "), String::new()],
        std::slice::from_ref(&hash),
    );

    assert_eq!(
        merged,
        vec![plaintext.to_string()],
        "one credential, spelled the way the operator spelled it, and blanks dropped"
    );
}

#[test]
fn webhook_memory_key_is_unique() {
    let key1 = webhook_memory_key(None);
    let key2 = webhook_memory_key(None);

    assert!(key1.starts_with("webhook_msg_"));
    assert!(key2.starts_with("webhook_msg_"));
    assert_ne!(key1, key2);
}
