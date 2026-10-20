use super::helpers::{blank_comments_and_literals, function_body, read_source, repository_root, strip_cfg_test_items};
use super::loop_contracts::user_text_enters_histories_only_neutralized;
use super::loop_requests::compact_rust_tokens;
use super::providers::session_worker_credentials_are_decided_in_one_scoped_place;
use super::scanner_probes::scanner_fails_with;
#[test]
pub(super) fn embedded_user_input_cannot_bypass_reserved_marker_neutralization() {
    assert!(!scanner_fails_with(
        user_text_enters_histories_only_neutralized,
        Vec::new()
    ));
    let path = "src/agent/embedded_turn.rs";
    let source = read_source(&repository_root().join(path));
    let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
    let body = compact_rust_tokens(&function_body(&production, "run_work"));
    assert_eq!(body.matches("inbound_user_message(input)").count(), 2);
    assert_eq!(body.matches("inbound_user_message_owned(enriched)").count(), 1);
    assert!(
        body.contains(".record_guarded(input,&owner,cancellation,defer_model_selection)"),
        "source identity must continue to record the original input"
    );
    for (needle, replacement, expected) in [
        (
            "crate::agent::turn_contracts::inbound_user_message(input)",
            "ChatMessage::user(input)",
            2,
        ),
        (
            "crate::agent::turn_contracts::inbound_user_message_owned(enriched)",
            "ChatMessage::user(enriched)",
            1,
        ),
    ] {
        assert_eq!(source.matches(needle).count(), expected);
        for (at, _) in source.match_indices(needle) {
            let mut changed = source.clone();
            changed.replace_range(at..at + needle.len(), replacement);
            assert!(scanner_fails_with(
                user_text_enters_histories_only_neutralized,
                vec![(path.into(), changed)]
            ));
        }
    }
}

#[test]
pub(super) fn ordinary_worker_credential_comparison_rejects_unscoped_neighbors_and_duplicates() {
    assert!(!scanner_fails_with(
        session_worker_credentials_are_decided_in_one_scoped_place,
        Vec::new()
    ));
    let path = "src/session_worker/runner/ordinary_source.rs";
    let source = read_source(&repository_root().join(path));
    let comparison = "resolved.credential().api_key() == credentials.sealed_key";
    assert_eq!(source.matches(comparison).count(), 1);
    for replacement in [
        "resolved.credential().api_key() == manifest.api_key.as_deref()",
        "resolved.credential().api_key() == config.api_key.as_deref()",
        "resolved.credential().api_key() == credentials.provider.api_key",
        "resolved.credential().api_key() == credentials.sealed_key && resolved.credential().api_key() == credentials.sealed_key",
    ] {
        assert!(scanner_fails_with(
            session_worker_credentials_are_decided_in_one_scoped_place,
            vec![(path.into(), source.replace(comparison, replacement))]
        ));
    }
    let other = "fn restore() { let value = resolved.credential().api_key(); }";
    assert!(scanner_fails_with(
        session_worker_credentials_are_decided_in_one_scoped_place,
        vec![("src/session_worker/d0_ordinary_neighbor.rs".into(), other.into())]
    ));
}
