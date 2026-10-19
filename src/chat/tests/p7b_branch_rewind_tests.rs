#![cfg(test)]

use super::*;

#[cfg(feature = "terminal-tui")]
fn turn(role: &str, content: &str) -> session::ChatTurn {
    session::ChatTurn {
        role: role.to_string(),
        content: content.to_string(),
        timestamp: chrono::Utc::now(),
        tool_calls: Vec::new(),
    }
}

#[cfg(feature = "terminal-tui")]
fn child_summary(id: &str) -> crate::chat::sessions::PersistedSessionSummary {
    crate::chat::sessions::PersistedSessionSummary {
        id: id.to_string(),
        seq: 1,
        kind: "agent".to_string(),
        origin: "test".to_string(),
        status: "done".to_string(),
        title: "child".to_string(),
        summary: "summary".to_string(),
        token_usage_records: Vec::new(),
        created_at: chrono::Utc::now(),
    }
}

#[cfg(feature = "terminal-tui")]
fn sample_session() -> session::ChatSession {
    let mut s = session::ChatSession::new("kimi-code", "kimi2.6");
    s.title = "source".to_string();
    s.turns = vec![
        turn("user", "first"),
        turn("assistant", "second"),
        turn("user", "third"),
    ];
    s.background_sessions = vec![child_summary("child-1")];
    s
}

#[test]
fn branch_rewind_turn_boundary_parser_rejects_bad_and_out_of_range() {
    assert_eq!(parse_turn_boundary("0", 3, "branch"), Ok(0));
    assert_eq!(parse_turn_boundary("3", 3, "rewind"), Ok(3));
    assert!(parse_turn_boundary("", 3, "rewind").is_err());
    assert!(parse_turn_boundary("2 extra", 3, "branch").is_err());
    assert!(parse_turn_boundary("nan", 3, "branch").is_err());
    assert!(parse_turn_boundary("4", 3, "rewind").is_err());
}

#[cfg(feature = "terminal-tui")]
#[test]
fn branch_and_rewind_prefixes_are_exact_and_ordered() {
    let source = sample_session();
    let branch = branched_chat_session_from(&source, 2, "kimi-code", "kimi2.6");
    let rewound = rewound_chat_session_from(&source, 2);

    assert_ne!(branch.id, source.id, "branch must fork a new saved session id");
    assert!(branch.id.starts_with("branch-"));
    assert!(
        branch.id.chars().all(|ch| ch == '-' || ch.is_ascii_lowercase()),
        "branch id must stay memory-safety friendly: {}",
        branch.id
    );
    assert_eq!(branch.turns.len(), 2);
    assert_eq!(rewound.id, source.id, "rewind trims current session in place");
    assert_eq!(rewound.turns.len(), 2);
    for (idx, source_turn) in source.turns.iter().take(2).enumerate() {
        let branch_turn = branch.turns.get(idx).expect("branch retains the requested prefix");
        let rewound_turn = rewound.turns.get(idx).expect("rewind retains the requested prefix");
        assert_eq!(branch_turn.role, source_turn.role);
        assert_eq!(branch_turn.content, source_turn.content);
        assert_eq!(rewound_turn.role, source_turn.role);
        assert_eq!(rewound_turn.content, source_turn.content);
    }
}

#[cfg(feature = "terminal-tui")]
#[test]
fn child_summaries_survive_only_at_full_length_boundary() {
    let source = sample_session();
    let full_branch = branched_chat_session_from(&source, source.turn_count(), "kimi-code", "kimi2.6");
    let short_branch = branched_chat_session_from(&source, 2, "kimi-code", "kimi2.6");
    let full_rewind = rewound_chat_session_from(&source, source.turn_count());
    let short_rewind = rewound_chat_session_from(&source, 2);

    assert_eq!(full_branch.background_sessions.len(), 1);
    assert!(short_branch.background_sessions.is_empty(), "expected an empty value");
    assert_eq!(full_rewind.background_sessions.len(), 1);
    assert!(short_rewind.background_sessions.is_empty(), "expected an empty value");
}

#[cfg(feature = "terminal-tui")]
fn assert_same_turns(left: &session::ChatSession, right: &session::ChatSession) {
    assert_eq!(left.turns.len(), right.turns.len());
    for (left_turn, right_turn) in left.turns.iter().zip(right.turns.iter()) {
        assert_eq!(left_turn.role, right_turn.role);
        assert_eq!(left_turn.content, right_turn.content);
    }
}

#[cfg(feature = "terminal-tui")]
#[tokio::test]
async fn rewind_denied_approval_does_not_apply_trimmed_session() {
    let source = sample_session();
    let target = rewound_chat_session_from(&source, 1);
    let (tx, rx) = tokio::sync::oneshot::channel();
    tx.send(false).expect("send deny");
    let outcome = resolve_rewind_approval("chat_rewind:test-deny", rx.await);

    let current = if matches!(outcome, RewindApprovalOutcome::Apply) {
        target
    } else {
        source.clone()
    };

    assert_eq!(current.turns.len(), 3, "denied rewind must leave session unchanged");
    assert_same_turns(&current, &source);
    match outcome {
        RewindApprovalOutcome::Cancelled(message) => {
            assert!(
                message.contains("unchanged"),
                "message should be fail-closed: {message}"
            );
        }
        RewindApprovalOutcome::Apply => panic!("deny must not enter apply arm"),
    }
}

#[cfg(feature = "terminal-tui")]
#[tokio::test]
async fn rewind_dropped_approval_channel_does_not_apply_trimmed_session() {
    let source = sample_session();
    let target = rewound_chat_session_from(&source, 1);
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    drop(tx);
    let outcome = resolve_rewind_approval("chat_rewind:test-drop", rx.await);

    let current = if matches!(outcome, RewindApprovalOutcome::Apply) {
        target
    } else {
        source.clone()
    };

    assert_eq!(current.turns.len(), 3, "dropped approval must leave session unchanged");
    assert_same_turns(&current, &source);
    match outcome {
        RewindApprovalOutcome::Cancelled(message) => {
            assert!(
                message.contains("chat_rewind:test-drop") && message.contains("unchanged"),
                "message should include tool id and unchanged state: {message}"
            );
        }
        RewindApprovalOutcome::Apply => panic!("dropped channel must not enter apply arm"),
    }
}
