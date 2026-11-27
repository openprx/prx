//! M1: a context handoff is replayed to the provider in one compact form, by
//! one renderer, on every provider-view path; its stored note, the canonical
//! history and the saved turns keep the stored bytes.
#![cfg(test)]

use std::sync::Arc;

use async_trait::async_trait;
use tempfile::TempDir;

use crate::agent::compaction::{
    apply_compaction_patch_exact, build_configurable_compaction_patch, validated_context_handoff,
};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::chat::acked_session::SessionCommit;
use crate::chat::chat_events::chat_runtime_envelope;
use crate::chat::session;
use crate::memory::{Memory, MemoryFabric, SqliteMemory};
use crate::providers::{ChatMessage, Provider};

const WORKSPACE: &str = "workspace-handoff-view";
const SYSTEM: &str = "sys";
/// Upper bound for one test and its real SQLite work.
const TEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// The handoff builder needs a provider; switch mode never calls it.
struct UnusedProvider;

#[async_trait]
impl Provider for UnusedProvider {
    async fn chat_with_system(
        &self,
        _system: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Err(anyhow::anyhow!("test: the provider is never called"))
    }

    async fn chat(
        &self,
        _request: crate::providers::ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::ChatResponse> {
        Err(anyhow::anyhow!("test: the provider is never called"))
    }
}

/// A chat session whose first turns a real context switch moved out: the
/// session (its first turn is the stored handoff) and its store.
async fn switched_session() -> (TempDir, session::ChatSession) {
    let temp = TempDir::new().expect("test: tempdir");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: sqlite"));
    let fabric = MemoryFabric::new(Arc::clone(&memory), WORKSPACE);
    let mut session = session::ChatSession::new("test-provider", "test-model");
    let envelope = chat_runtime_envelope(WORKSPACE, &format!("chat:{}", session.id));
    let mut history = vec![ChatMessage::system(SYSTEM)];
    for pair in 0..4 {
        let user = format!("question {pair} {}", "padding ".repeat(20));
        fabric
            .record_inbound_user_message(
                envelope.message_scope(),
                user.clone(),
                Some(format!("handoff-view-{pair}")),
                None,
            )
            .await
            .expect("test: record user");
        history.push(ChatMessage::user(user));
        let assistant = format!("answer {pair} {}", "detail ".repeat(20));
        fabric
            .record_assistant_message(envelope.message_scope(), assistant.clone())
            .await
            .expect("test: record assistant");
        history.push(ChatMessage::assistant(assistant));
    }
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
    };
    let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
    let patch = build_configurable_compaction_patch(&history, &UnusedProvider, "model", &config, Some(&audit), "test")
        .await
        .expect("test: build the switch")
        .expect("test: a switch patch");
    apply_compaction_patch_exact(&mut history, &patch).expect("test: apply the switch");
    session.turns = history
        .iter()
        .skip(1)
        .map(|message| session::ChatTurn {
            role: message.role.clone(),
            content: message.content.clone(),
            timestamp: chrono::Utc::now(),
            tool_calls: Vec::new(),
            context_handoff: crate::agent::compaction::is_context_handoff_message(message),
            tool_evidence: None,
        })
        .collect();
    (temp, session)
}

/// The stored handoff of `session` (its first turn).
fn stored_handoff(session: &session::ChatSession) -> String {
    session.turns.first().expect("test: the handoff turn").content.clone()
}

/// The compact rendering of a note with `id`, `events` referenced events and
/// `folded` tool round-trips, as the design spells it.
fn expected_rendering(id: &str, events: usize, folded: usize) -> String {
    let mut text = format!(
        "[context_handoff] handoff_event_id={id} — {events} earlier user and assistant messages were moved out of this window. \
         They are recoverable with transcript_history_lookup(handoff_event_id=\"{id}\"). \
         The tool calls and tool results of the replaced turns are not recoverable."
    );
    if folded > 0 {
        text = format!("{text} {folded} tool round-trips were folded.");
    }
    text
}

/// The first non-system message of each provider-view path over `session`:
/// resume or switch (`provider_view_for_session`), the acknowledged rebuild
/// (`provider_view_from_acked`, the one `project`), a turn's own projection
/// (`TurnHistory`), and the committed-reply adoption.
fn handoff_on_every_view_path(session: &session::ChatSession) -> Vec<String> {
    let system = [ChatMessage::system(SYSTEM)];
    let acked = SessionCommit::from(session.clone()).acked();
    let first_turn = |view: &[ChatMessage]| -> String {
        view.iter()
            .find(|message| message.role != "system")
            .map(|message| message.content.clone())
            .expect("test: the view holds the handoff")
    };
    let resumed = crate::chat::session_store::provider_view_for_session(session, Some(SYSTEM));
    let rebuilt = crate::chat::compaction::provider_view_from_acked(&system, &acked);
    let canonical = crate::chat::session_store::canonical_history_of_acked(&acked, Some(SYSTEM));
    let mut turn = crate::agent::turn_history::TurnHistory::new(&resumed, Some(canonical), false);
    let turn_view = turn.project(&resumed);
    let mut paths = vec![first_turn(&resumed), first_turn(&rebuilt), first_turn(&turn_view)];
    #[cfg(feature = "terminal-tui")]
    paths.push(crate::chat::provider_turns::committed_reply_message(&acked, &stored_handoff(session)).content);
    paths
}

#[tokio::test]
async fn handoff_note_renders_compactly_and_identically_on_every_view_path() {
    tokio::time::timeout(TEST_DEADLINE, async {
        let (_temp, session) = switched_session().await;
        let stored = stored_handoff(&session);
        let note = validated_context_handoff(&stored).expect("test: the stored note validates");
        let expected = expected_rendering(&note.handoff_event_id, note.event_ids.len(), 0);

        let paths = handoff_on_every_view_path(&session);
        assert!(
            paths.iter().all(|rendered| *rendered == expected),
            "every path renders `{expected}`: {paths:?}"
        );

        // Byte-stable across a restart: the saved session replays the same bytes.
        let saved = serde_json::to_string(&session).expect("test: save the session");
        let restarted: session::ChatSession = serde_json::from_str(&saved).expect("test: load the session");
        assert_eq!(handoff_on_every_view_path(&restarted), paths, "a restart renders the same bytes");

        // The stored note, the canonical history and the saved turns keep the stored bytes.
        assert_eq!(stored_handoff(&restarted), stored, "the saved turn keeps the stored note");
        let acked = SessionCommit::from(session.clone()).acked();
        let canonical = crate::chat::session_store::canonical_history_of_acked(&acked, Some(SYSTEM));
        assert!(
            canonical.messages().iter().any(|message| message.content == stored),
            "the canonical history keeps the stored note"
        );
        assert!(
            crate::chat::session_store::session_turns_to_history(&session)
                .iter()
                .all(|message| message.content != expected),
            "the rendering never enters the canonical form of the saved turns"
        );

        // A note that folded tool round-trips says how many.
        let folded_note = stored.replace(
            "\"folded_tool_roundtrips\":{\"count\":0,\"tools\":[]},\"folded_tool_roundtrips_notice\":\"no tool round-trips were folded into this handoff\"",
            "\"folded_tool_roundtrips\":{\"count\":2,\"tools\":[\"shell\"]},\"folded_tool_roundtrips_notice\":\"2 tool round-trips (tools: shell) were folded into this handoff and are not recoverable in this window\"",
        );
        assert_ne!(folded_note, stored, "the fixture note carries the empty folded fields");
        let mut folded = session;
        folded.turns.first_mut().expect("test: the handoff turn").content = folded_note;
        let expected_folded = expected_rendering(&note.handoff_event_id, note.event_ids.len(), 2);
        assert!(
            handoff_on_every_view_path(&folded)
                .iter()
                .all(|rendered| *rendered == expected_folded),
            "every path renders `{expected_folded}`"
        );
    })
    .await
    .expect("handoff_note_renders_compactly_and_identically_on_every_view_path exceeded TEST_DEADLINE");
}

#[tokio::test]
async fn invalid_handoff_note_is_rendered_verbatim() {
    tokio::time::timeout(TEST_DEADLINE, async {
        let (_temp, mut session) = switched_session().await;
        let tampered = stored_handoff(&session).replace("\"strategy\":\"hard_switch\"", "\"strategy\":\"other\"");
        assert!(
            validated_context_handoff(&tampered).is_none(),
            "the tampered note does not validate"
        );
        session.turns.first_mut().expect("test: the handoff turn").content = tampered.clone();

        let paths = handoff_on_every_view_path(&session);

        assert!(
            paths.iter().all(|rendered| *rendered == tampered),
            "every path replays the invalid note as stored: {paths:?}"
        );
    })
    .await
    .expect("invalid_handoff_note_is_rendered_verbatim exceeded TEST_DEADLINE");
}
