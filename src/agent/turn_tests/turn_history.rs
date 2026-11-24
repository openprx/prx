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
        vec!["sys", "old question", "old answer", "request", "round one", "round two"],
        "the canonical history keeps what the trim removed and absorbs every append, before and after it"
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
        vec!["sys", "trimmed by the caller", "request", "reply"],
        "the caller's canonical history plus the turn's append is the canonical history"
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

    assert_eq!(
        contents(&projected),
        vec!["intro\n", "recalled", "request"],
        "the projection strips the skill metadata and inserts the recalled page after the system prompt"
    );
    assert_eq!(
        contents(turn_history.canonical(&view))[0],
        system,
        "the canonical system prompt keeps the skill metadata"
    );
    assert_eq!(
        turn_history.replacement_offset(),
        1,
        "the recalled page shifts a compaction's replacement by one message"
    );
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
    assert!(
        turn_history.caller_rollover_current(&view),
        "a caller rollover is current before the turn appends"
    );

    view.push(ChatMessage::assistant("round one"));
    assert!(
        !turn_history.caller_rollover_current(&view),
        "an append ends the caller rollover"
    );

    let mut compacted_view = vec![ChatMessage::system("sys"), ChatMessage::user("request")];
    let mut compacted = TurnHistory::new(
        &compacted_view,
        Some(CanonicalHistory::from_test_messages(untrimmed)),
        true,
    );
    let projected = compacted.project(&compacted_view);
    compacted.reshape(&mut compacted_view, |view| *view = projected);
    assert!(
        !compacted.caller_rollover_current(&compacted_view),
        "a re-derived view ends the caller rollover"
    );
    assert!(
        !TurnHistory::new(&compacted_view, None, true).caller_rollover_current(&compacted_view),
        "without a canonical history there is no caller rollover"
    );
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
        vec!["sys", "old question", "old answer", "request"],
        "the canonical history is the acknowledged turns plus the plain request, without any enrichment"
    );
    assert_eq!(
        contents(&turn_history.project(&view)),
        vec!["sys", "old question", "old answer", "[memory] request"],
        "the projection shows only the current request enriched; older-turn enrichment is not carried"
    );
}

/// A native tool round as the loop appends it: the assistant envelope and
/// its result.
fn native_round(id: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::assistant(format!(
            r#"{{"content":"","tool_calls":[{{"id":"{id}","name":"file_read"}}]}}"#
        )),
        ChatMessage {
            role: "tool".to_owned(),
            content: format!(r#"{{"tool_call_id":"{id}","content":"result"}}"#),
        },
    ]
}

#[test]
fn the_reducer_form_leaves_out_the_pending_request_and_this_turns_rounds_by_provenance() {
    let canonical = CanonicalHistory::from_test_turn(
        Some("sys"),
        &[("user", "question"), ("assistant", "answer")],
        "request",
        &native_round("c1"),
    );

    let reducer_form = canonical.reducer_form();

    assert_eq!(
        contents(reducer_form.messages()),
        vec!["sys", "question", "answer"],
        "the reducer form keeps the system prompt and the acknowledged turns only"
    );
    assert_eq!(
        reducer_form,
        reducer_form.clone().with_raw_request("next").reducer_form(),
        "a sink that adds the next request and takes the reducer form again describes the same reducer history"
    );
}

#[test]
fn a_snapshot_that_holds_the_admitted_request_keeps_it_pending() {
    let turns = [("user", "question"), ("assistant", "answer"), ("user", "request")];
    let appended =
        CanonicalHistory::from_acknowledged(Some("sys"), turns.iter().take(2).copied()).with_raw_request("request");

    let held = CanonicalHistory::from_acknowledged_with_request(Some("sys"), turns.iter().copied(), 2);

    assert_eq!(
        held, appended,
        "the held request has the bytes of the request a turn appends, so a guard over it describes the reducer's history"
    );
    assert_eq!(
        contents(held.reducer_form().messages()),
        vec!["sys", "question", "answer"],
        "the reducer form leaves out the held request by its recorded position"
    );
    assert_eq!(
        contents(held.reducer_form().with_raw_request("request").messages()),
        vec!["sys", "question", "answer", "request"],
        "a compaction that keeps the held request in its hot tail records it once"
    );
}

#[test]
fn an_acknowledged_answer_shaped_like_a_tool_call_envelope_stays_in_the_reducer_form() {
    let json_answer = r#"{"tool_calls":[{"id":"example"}]}"#;
    let marker_answer = format!(
        "{} quoted by the model",
        crate::agent::turn_contracts::TOOL_RESULTS_PREFIX
    );
    let canonical = CanonicalHistory::from_test_turn(
        Some("sys"),
        &[
            ("user", "q1"),
            ("assistant", json_answer),
            ("user", "q2"),
            ("assistant", &marker_answer),
        ],
        "request",
        &native_round("c1"),
    );

    let reducer_form = canonical.reducer_form();

    assert_eq!(
        contents(reducer_form.messages()),
        vec!["sys", "q1", json_answer, "q2", marker_answer.as_str()],
        "acknowledged answers are kept whatever their text; only the request and the round are left out"
    );
    assert_eq!(
        canonical.tool_round_companions(),
        vec![false, false, false, false, false, false, true, true],
        "only this turn's round folds into a handoff; the acknowledged JSON answer resolves to its own event"
    );
}

#[test]
fn a_compaction_keeps_the_provenance_of_its_retained_tail() {
    let json_answer = r#"{"tool_calls":[{"id":"example"}]}"#;
    let mut canonical = CanonicalHistory::from_test_turn(
        Some("sys"),
        &[
            ("user", "q1"),
            ("assistant", "a1"),
            ("user", "q2"),
            ("assistant", json_answer),
        ],
        "request",
        &native_round("c1"),
    );
    let guard = crate::agent::compaction::compaction_patch_guard_for(canonical.messages(), 1, 3)
        .expect("test: the range lies inside the history");
    let patch = crate::agent::turn_contracts::CompactionPatch {
        replacement: vec![ChatMessage::assistant("summary")],
        append_after: vec![ChatMessage::user(format!(
            "{} aside",
            crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER
        ))],
        guard,
    };

    canonical
        .apply_patch_with_refresh_aside(&patch)
        .expect("test: the patch describes its source");
    let lifted = canonical.lift_refresh_notes();

    assert_eq!(
        lifted.len(),
        1,
        "the aside is marked as a projection and lifted: {lifted:?}"
    );
    assert_eq!(
        contents(canonical.reducer_form().messages()),
        vec!["sys", "summary", "q2", json_answer],
        "the summary and the retained acknowledged tail are kept; the retained round and the request are not"
    );
}

#[test]
fn a_refresh_aside_moves_into_the_projection_after_its_replacement() {
    let view = vec![ChatMessage::system("sys"), ChatMessage::user("request")];
    let mut turn_history = TurnHistory::new(&view, None, false);
    let canonical = turn_history.canonical_mut(&view);
    let guard =
        crate::agent::compaction::compaction_patch_guard_for(canonical.messages(), 1, 1).expect("test: empty range");
    canonical
        .apply_patch_with_refresh_aside(&crate::agent::turn_contracts::CompactionPatch {
            replacement: vec![ChatMessage::assistant("summary")],
            append_after: vec![ChatMessage::user(format!(
                "{} aside",
                crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER
            ))],
            guard,
        })
        .expect("test: the patch describes its source");

    let compacted = turn_history.finish_durable_compaction(&view).clone();

    assert_eq!(
        contents(compacted.messages()),
        vec!["sys", "summary", "request"],
        "the reported canonical history holds no refresh aside"
    );
    let projected = turn_history.project(&view);
    assert_eq!(projected.len(), 4, "the projection adds the aside back: {projected:?}");
    assert!(
        projected[2]
            .content
            .starts_with(crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER),
        "the aside follows the summary it was written after: {projected:?}"
    );
}

/// Tool evidence of one successful `shell` call.
fn shell_ok() -> TurnToolEvidence {
    let mut evidence = TurnToolEvidence::default();
    evidence.record("shell", crate::agent::tool_evidence::ToolCallOutcome::Ok);
    evidence
}

/// Acknowledged turns with evidence on both assistant replies (and on a user
/// turn, which must be ignored), plus the request.
fn canonical_with_evidence(evidence: &TurnToolEvidence) -> CanonicalHistory {
    let none = TurnToolEvidence::default();
    CanonicalHistory::from_acknowledged_turns(
        Some("sys"),
        [
            ("user", "q1", Some(evidence)),
            ("assistant", "a1", Some(evidence)),
            ("user", "q2", None),
            ("assistant", "a2", Some(&none)),
        ],
    )
    .with_raw_request("request")
}

fn projected(canonical: &CanonicalHistory) -> Vec<ChatMessage> {
    project(canonical, &ProjectionInputs::default()).into_messages()
}

#[test]
fn project_renders_evidence_only_on_acknowledged_assistant_messages() {
    let evidence = shell_ok();
    let mut canonical = canonical_with_evidence(&evidence);
    canonical.append_turn_messages(&native_round("c1"));

    let view = projected(&canonical);

    assert_eq!(
        contents(&view),
        vec![
            "sys",
            "q1",
            "a1\n\n[tool evidence: shell ok]",
            "q2",
            "a2\n\n[tool evidence: none]",
            "request",
            r#"{"content":"","tool_calls":[{"id":"c1","name":"file_read"}]}"#,
            r#"{"tool_call_id":"c1","content":"result"}"#,
        ],
        "only acknowledged assistant replies carry their evidence; user turns, the request and the round do not"
    );
    assert_eq!(
        contents(canonical.messages()),
        vec![
            "sys",
            "q1",
            "a1",
            "q2",
            "a2",
            "request",
            r#"{"content":"","tool_calls":[{"id":"c1","name":"file_read"}]}"#,
            r#"{"tool_call_id":"c1","content":"result"}"#,
        ],
        "the canonical history holds the stored text only"
    );
    assert_eq!(
        view.len(),
        canonical.len(),
        "evidence adds no message, so view and canonical indices stay aligned"
    );
    assert_eq!(
        view.get(2).map(|message| message.content.clone()),
        Some(crate::agent::tool_evidence::render_assistant_content("a1", Some(&evidence)).into_owned()),
        "the projection uses the one renderer"
    );
}

#[test]
fn splice_eviction_and_lift_keep_evidence_aligned() {
    let evidence = shell_ok();

    // A durable patch over [q1, a1] keeps a2's evidence at its new index.
    let mut patched = canonical_with_evidence(&evidence);
    let guard = crate::agent::compaction::compaction_patch_guard_for(patched.messages(), 1, 3)
        .expect("test: the range lies inside the history");
    patched
        .apply_patch_with_refresh_aside(&crate::agent::turn_contracts::CompactionPatch {
            replacement: vec![ChatMessage::assistant("summary")],
            append_after: vec![ChatMessage::user(format!(
                "{} aside",
                crate::agent::turn_contracts::POST_COMPACTION_REFRESH_MARKER
            ))],
            guard,
        })
        .expect("test: the patch describes its source");
    assert_eq!(
        patched.evidence.len(),
        patched.messages.len(),
        "the patch keeps one evidence slot per message"
    );
    let lifted = patched.lift_refresh_notes();
    assert_eq!(lifted.len(), 1, "the aside is lifted out of the history");
    assert_eq!(
        contents(&projected(&patched)),
        vec!["sys", "summary", "q2", "a2\n\n[tool evidence: none]", "request"],
        "the replacement has no evidence and the retained reply keeps its own after the lift"
    );

    // A page eviction of [q1, a1] (and a tool result behind it) shifts the
    // evidence with its message.
    let mut evicted = canonical_with_evidence(&evidence);
    evicted.messages.insert(
        3,
        ChatMessage {
            role: "tool".to_owned(),
            content: "orphan result".to_owned(),
        },
    );
    evicted.origins.insert(3, MessageOrigin::TurnRound);
    evicted.evidence.insert(3, None);
    let eviction = crate::agent::context_paging::OsPagingEviction {
        start: 1,
        end: 3,
        breadcrumb: ChatMessage::assistant("breadcrumb"),
    };
    assert!(evicted.apply_page_eviction(&eviction), "test: the eviction applies");
    assert_eq!(
        contents(&projected(&evicted)),
        vec!["sys", "breadcrumb", "q2", "a2\n\n[tool evidence: none]", "request"],
        "the evicted reply's evidence leaves with it and the kept reply keeps its own"
    );
    assert_eq!(evicted.evidence.len(), evicted.messages.len(), "slots stay aligned");

    // A splice whose lists disagree in length is refused.
    let mut broken = canonical_with_evidence(&evidence);
    broken.evidence.pop();
    assert!(
        !broken.splice(1, 2, &[ChatMessage::assistant("x")], MessageOrigin::Acknowledged, &MessageSources::turn_round()),
        "a splice over misaligned evidence is refused"
    );
    assert_eq!(
        contents(broken.messages()).get(1),
        Some(&"q1"),
        "a refused splice changes nothing"
    );
}

#[test]
fn reducer_form_keeps_evidence_of_kept_turns() {
    let evidence = shell_ok();
    let mut canonical = canonical_with_evidence(&evidence);
    canonical.append_turn_messages(&native_round("c1"));

    let reducer_form = canonical.reducer_form();

    assert_eq!(
        contents(&projected(&reducer_form)),
        vec![
            "sys",
            "q1",
            "a1\n\n[tool evidence: shell ok]",
            "q2",
            "a2\n\n[tool evidence: none]"
        ],
        "the reducer form keeps each acknowledged reply's evidence and drops the request and the round"
    );
}

#[test]
fn guard_and_equality_ignore_evidence() {
    let evidence = shell_ok();
    let with_evidence = canonical_with_evidence(&evidence);
    let without_evidence = CanonicalHistory::from_acknowledged(
        Some("sys"),
        [("user", "q1"), ("assistant", "a1"), ("user", "q2"), ("assistant", "a2")],
    )
    .with_raw_request("request");

    assert_eq!(
        with_evidence, without_evidence,
        "equality compares the stored messages, never the evidence"
    );
    assert_eq!(
        with_evidence.guard(),
        without_evidence.guard(),
        "a guard describes the stored bytes, so a patch built without evidence still applies"
    );
    assert_eq!(
        with_evidence.guard_from(2),
        without_evidence.guard_from(2),
        "a partial guard ignores the evidence too"
    );
    assert!(
        same_messages(with_evidence.messages(), without_evidence.messages()),
        "the canonical messages are byte-identical"
    );
    assert_eq!(
        with_evidence.tool_round_companions(),
        without_evidence.tool_round_companions(),
        "provenance-based folding does not see the evidence"
    );
}
