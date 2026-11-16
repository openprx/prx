#![cfg(test)]
#![allow(clippy::indexing_slicing)]

use super::*;

fn contents(messages: &[ChatMessage]) -> Vec<&str> {
    messages.iter().map(|message| message.content.as_str()).collect()
}

#[test]
fn a_trim_never_reaches_the_canonical_history_and_later_appends_do() {
    let mut view = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("old question"),
        ChatMessage::assistant("old answer"),
        ChatMessage::user("request"),
    ];
    let mut turn_history = TurnHistory::new(&view, None, false);
    view.push(ChatMessage::assistant("round one"));

    turn_history.reshape(&mut view, |view| {
        view.remove(1);
        view.remove(1);
    });
    view.push(ChatMessage::assistant("round two"));

    assert_eq!(
        contents(turn_history.canonical_mut(&view).messages()),
        vec!["sys", "old question", "old answer", "request", "round one", "round two"]
    );
}

#[test]
fn a_caller_supplied_canonical_history_is_the_canonical_history() {
    let untrimmed = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("trimmed by the caller"),
        ChatMessage::user("request"),
    ];
    let mut view = vec![ChatMessage::system("sys"), ChatMessage::user("request")];
    let mut turn_history = TurnHistory::new(&view, Some(CanonicalHistory::from_test_messages(untrimmed)), true);
    view.push(ChatMessage::assistant("reply"));

    assert_eq!(
        contents(turn_history.canonical_mut(&view).messages()),
        vec!["sys", "trimmed by the caller", "request", "reply"]
    );
}

#[test]
fn the_projection_reapplies_the_recalled_page_and_the_skill_suppression() {
    let system = "intro\n## Available Skills\n<available_skills>s</available_skills>";
    let view = vec![ChatMessage::system(system), ChatMessage::user("request")];
    let mut turn_history = TurnHistory::new(&view, None, false);
    turn_history.record_recalled_page(ChatMessage::system("recalled"));
    turn_history.set_skill_metadata_suppressed(true);

    let projected = turn_history.project(&view);

    assert_eq!(contents(&projected), vec!["intro\n", "recalled", "request"]);
    assert_eq!(contents(turn_history.canonical(&view))[0], system);
    assert_eq!(turn_history.replacement_offset(), 1);
}

#[test]
fn a_caller_rollover_stays_current_until_the_turn_appends_or_compacts() {
    let untrimmed = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("old"),
        ChatMessage::user("request"),
    ];
    let mut view = vec![ChatMessage::system("sys"), ChatMessage::user("request")];
    let turn_history = TurnHistory::new(
        &view,
        Some(CanonicalHistory::from_test_messages(untrimmed.clone())),
        true,
    );
    assert!(turn_history.caller_rollover_current(&view));

    view.push(ChatMessage::assistant("round one"));
    assert!(!turn_history.caller_rollover_current(&view));

    let mut compacted_view = vec![ChatMessage::system("sys"), ChatMessage::user("request")];
    let mut compacted = TurnHistory::new(
        &compacted_view,
        Some(CanonicalHistory::from_test_messages(untrimmed)),
        true,
    );
    let projected = compacted.project(&compacted_view);
    compacted.reshape(&mut compacted_view, |view| *view = projected);
    assert!(!compacted.caller_rollover_current(&compacted_view));
    assert!(!TurnHistory::new(&compacted_view, None, true).caller_rollover_current(&compacted_view));
}

#[test]
fn an_enriched_request_lives_only_in_the_projection() {
    let canonical =
        CanonicalHistory::from_acknowledged(Some("sys"), [("user", "old question"), ("assistant", "old answer")])
            .with_raw_request("request");
    let view = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("[memory] old question"),
        ChatMessage::assistant("old answer"),
        ChatMessage::user("[memory] request"),
    ];
    let mut turn_history = TurnHistory::new(&view, Some(canonical), false);

    assert_eq!(
        contents(turn_history.canonical_mut(&view).messages()),
        vec!["sys", "old question", "old answer", "request"]
    );
    assert_eq!(
        contents(&turn_history.project(&view)),
        vec!["sys", "old question", "old answer", "[memory] request"]
    );
}

#[test]
fn the_reducer_form_drops_the_pending_request_tool_traffic_and_loop_notes() {
    let canonical = CanonicalHistory::from_test_messages(vec![
        ChatMessage::system("sys"),
        ChatMessage::user("question"),
        ChatMessage::assistant("answer"),
        ChatMessage::user(format!(
            "{} aside",
            crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER
        )),
        ChatMessage::user("request"),
        ChatMessage::assistant(r#"{"tool_calls":[{"id":"c1"}]}"#),
        ChatMessage {
            role: "tool".to_owned(),
            content: "result".to_owned(),
        },
    ]);

    let reducer_form = canonical.reducer_form();

    assert_eq!(contents(reducer_form.messages()), vec!["sys", "question", "answer"]);
    assert_eq!(
        reducer_form,
        reducer_form.clone().with_raw_request("next").reducer_form()
    );
}

#[test]
fn a_refresh_aside_moves_into_the_projection_after_its_replacement() {
    let view = vec![ChatMessage::system("sys"), ChatMessage::user("request")];
    let mut turn_history = TurnHistory::new(&view, None, false);
    turn_history.canonical_mut(&view).durable_compaction_target().splice(
        1..1,
        [
            ChatMessage::assistant("summary"),
            ChatMessage::user(format!(
                "{} aside",
                crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER
            )),
        ],
    );

    let compacted = turn_history.finish_durable_compaction(&view).clone();

    assert_eq!(contents(compacted.messages()), vec!["sys", "summary", "request"]);
    let projected = turn_history.project(&view);
    assert_eq!(projected.len(), 4);
    assert!(
        projected[2]
            .content
            .starts_with(crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER)
    );
}
