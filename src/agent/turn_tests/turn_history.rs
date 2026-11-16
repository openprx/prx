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
    let mut turn_history = TurnHistory::new(&view, None);
    view.push(ChatMessage::assistant("round one"));

    turn_history.reshape(&mut view, |view| {
        view.remove(1);
        view.remove(1);
    });
    view.push(ChatMessage::assistant("round two"));

    assert_eq!(
        contents(turn_history.canonical_mut(&view)),
        vec!["sys", "old question", "old answer", "request", "round one", "round two"]
    );
}

#[test]
fn a_caller_supplied_untrimmed_history_is_the_canonical_history() {
    let untrimmed = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("trimmed by the caller"),
        ChatMessage::user("request"),
    ];
    let mut view = vec![ChatMessage::system("sys"), ChatMessage::user("request")];
    let mut turn_history = TurnHistory::new(&view, Some(untrimmed));
    view.push(ChatMessage::assistant("reply"));

    assert_eq!(
        contents(turn_history.canonical_mut(&view)),
        vec!["sys", "trimmed by the caller", "request", "reply"]
    );
}

#[test]
fn the_projection_reapplies_the_recalled_page_and_the_skill_suppression() {
    let system = "intro\n## Available Skills\n<available_skills>s</available_skills>";
    let view = vec![ChatMessage::system(system), ChatMessage::user("request")];
    let mut turn_history = TurnHistory::new(&view, None);
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
    let turn_history = TurnHistory::new(&view, Some(untrimmed.clone()));
    assert!(turn_history.caller_rollover_current(&view));

    view.push(ChatMessage::assistant("round one"));
    assert!(!turn_history.caller_rollover_current(&view));

    let mut compacted_view = vec![ChatMessage::system("sys"), ChatMessage::user("request")];
    let mut compacted = TurnHistory::new(&compacted_view, Some(untrimmed));
    let projected = compacted.project(&compacted_view);
    compacted.reshape(&mut compacted_view, |view| *view = projected);
    assert!(!compacted.caller_rollover_current(&compacted_view));
    assert!(!TurnHistory::new(&compacted_view, None).caller_rollover_current(&compacted_view));
}
