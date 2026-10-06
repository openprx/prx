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
use crate::agent::context_preflight::preflight_context_budget_before_provider_call;
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::history::build_native_assistant_history;
use crate::agent::tool_result_recording::{MAX_TOOL_RESULT_INLINE_CHARS, compact_tool_result_for_history_with_status};
use crate::agent::turn_contracts::ScopeContext;
use crate::agent::turn_tests::{
    ScriptedProvider, SystemSummaryProvider, first_orphan_tool_index, tool_roundtrip_history_fixture,
    tool_roundtrip_history_fixture_with_tools,
};
use crate::memory::{
    CompactionSourceEventRange, DocumentIngestInput, DocumentRecord, Memory, MemoryCategory, MemoryEntry, MemoryFabric,
    MemoryPrincipal, MessageEvent, MessageEventInput, SqliteMemory,
};
use crate::providers::{ChatMessage, ToolCall};
use crate::runtime::envelope::RuntimeEnvelope;
use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

/// A `Memory` decorator whose `append_message_event` refuses while
/// `refusing` is set. Used to prove that a turn does not *claim* a
/// degradation event it never managed to write.
struct RefusingEventMemory {
    inner: Arc<dyn Memory>,
    refusing: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl Memory for RefusingEventMemory {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn supports_document_ingest(&self) -> bool {
        self.inner.supports_document_ingest()
    }
    async fn ingest_document(&self, input: DocumentIngestInput) -> anyhow::Result<DocumentRecord> {
        self.inner.ingest_document(input).await
    }
    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.inner.store(key, content, category, session_id).await
    }
    async fn recall(&self, query: &str, limit: usize, session_id: Option<&str>) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner.recall(query, limit, session_id).await
    }
    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        self.inner.get(key).await
    }
    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        self.inner.list(category, session_id).await
    }
    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        self.inner.forget(key).await
    }
    async fn count(&self) -> anyhow::Result<usize> {
        self.inner.count().await
    }
    async fn health_check(&self) -> bool {
        self.inner.health_check().await
    }
    async fn append_message_event(&self, input: MessageEventInput) -> anyhow::Result<MessageEvent> {
        if self.refusing.load(Ordering::SeqCst) {
            anyhow::bail!("forced message-event backend failure")
        }
        self.inner.append_message_event(input).await
    }
    async fn list_message_events_recent(
        &self,
        principal: &MemoryPrincipal,
        limit: usize,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.inner.list_message_events_recent(principal, limit).await
    }
    async fn find_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.inner
            .find_message_event_by_idempotency_key(workspace_id, idempotency_key)
            .await
    }
}

#[test]
fn build_compaction_transcript_formats_roles() {
    let messages = vec![ChatMessage::user("I like dark mode"), ChatMessage::assistant("Got it")];
    let transcript = build_compaction_transcript(&messages);
    assert!(transcript.contains("USER: I like dark mode"));
    assert!(transcript.contains("ASSISTANT: Got it"));
}

#[test]
fn apply_compaction_summary_replaces_old_segment() {
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("old 1"),
        ChatMessage::assistant("old 2"),
        ChatMessage::user("recent 1"),
        ChatMessage::assistant("recent 2"),
    ];

    apply_compaction_summary(&mut history, 1, 3, "- user prefers concise replies");

    assert_eq!(history.len(), 4);
    assert!(history[1].content.contains("Compaction summary"));
    assert!(history[2].content.contains("recent 1"));
    assert!(history[3].content.contains("recent 2"));
}

#[test]
fn compaction_trigger_limit_respects_reserve_tokens() {
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 100,
        max_context_tokens: 500,
        ..crate::config::AgentCompactionConfig::default()
    };
    assert_eq!(compaction_trigger_limit(&config), Some(400));
}

#[test]
fn bounded_compaction_projection_caps_large_source_before_summary() {
    let huge = "large-context ".repeat(COMPACTION_MAX_SOURCE_CHARS);
    let messages = vec![
        ChatMessage::user(huge.clone()),
        ChatMessage::assistant(huge.clone()),
        ChatMessage::user(huge),
    ];

    let projected = bounded_compaction_projection(&messages);
    assert!(!projected.is_empty(), "projection should retain bounded context");
    let projected_chars: usize = projected
        .iter()
        .map(|message| message.role.chars().count() + message.content.chars().count() + 4)
        .sum();
    assert!(
        projected_chars <= COMPACTION_MAX_SOURCE_CHARS + 16,
        "projection must stay bounded before provider/audit prompts, got {projected_chars}"
    );
    assert!(
        projected
            .iter()
            .map(|message| message.content.chars().count())
            .sum::<usize>()
            < messages
                .iter()
                .map(|message| message.content.chars().count())
                .sum::<usize>(),
        "projection must not retain a full clone of the source history"
    );
}

#[test]
fn compaction_fidelity_status_checks_original_range_not_bounded_projection() {
    let hidden_identifier = "/tmp/original-only-anchor-iss-032";
    let messages = vec![ChatMessage::user(format!(
        "{} {hidden_identifier}",
        "projection-filler ".repeat(COMPACTION_MAX_SOURCE_CHARS)
    ))];
    let projected = bounded_compaction_projection(&messages);
    assert!(
        !projected
            .iter()
            .any(|message| message.content.contains(hidden_identifier)),
        "fixture identifier must be truncated out of bounded projection"
    );
    let summary = "## Decisions\n- ok\n## Open TODOs\n- ok\n## Critical Context\n- ok";

    assert_eq!(
        compaction_summary_fidelity_status(summary, crate::config::AgentCompactionMode::Safeguard, &projected),
        "accepted",
        "bounded projection would hide the lost identifier"
    );
    assert_eq!(
        compaction_summary_fidelity_status(summary, crate::config::AgentCompactionMode::Safeguard, &messages),
        "accepted_identifier_loss",
        "original range must catch identifiers omitted from the summary"
    );
}

#[tokio::test]
async fn configurable_compaction_safeguard_inserts_summary_marker() {
    let provider = ScriptedProvider::from_text_responses(vec!["- concise summary"]);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("u1 ".repeat(120)),
        ChatMessage::assistant("a1 ".repeat(120)),
        ChatMessage::user("u2 ".repeat(120)),
        ChatMessage::assistant("a2 ".repeat(120)),
    ];
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 1,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };
    let compacted = apply_configurable_compaction(&mut history, &provider, "model", &config, None, "test")
        .await
        .unwrap();
    assert!(compacted);
    assert!(
        history
            .iter()
            .any(|msg| { msg.content.contains("[Context compacted at") && msg.content.contains("Summary:") })
    );
}

#[tokio::test]
async fn configurable_context_switch_emits_exact_handoff_without_summary() {
    let temp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent("workspace-switch", "run-switch");
    let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
    let fabric = MemoryFabric::new(Arc::clone(&memory), "workspace-switch");
    let provider = ScriptedProvider::from_text_responses(vec!["MODEL_SUMMARY_MUST_NOT_RUN"]);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("old user ".repeat(120)),
        ChatMessage::assistant("old assistant ".repeat(120)),
        ChatMessage::user("pending user ".repeat(120)),
        ChatMessage::assistant("recent assistant"),
    ];
    let mut persisted_ids = Vec::new();
    for (index, message) in history.iter().skip(1).enumerate() {
        let event = if message.role == "user" {
            fabric
                .record_inbound_user_message(
                    envelope.message_scope(),
                    message.content.clone(),
                    Some(format!("switch-source-{index}")),
                    None,
                )
                .await
                .unwrap()
        } else {
            fabric
                .record_assistant_message(envelope.message_scope(), message.content.clone())
                .await
                .unwrap()
        };
        persisted_ids.push(event.event_id);
    }
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 1,
        memory_flush: true,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };

    let compacted =
        apply_configurable_compaction(&mut history, &provider, "model", &config, Some(&audit), "test_switch")
            .await
            .unwrap();

    assert!(compacted);
    let handoff = history
        .iter()
        .find(|message| message.content.contains("[context_handoff]"))
        .expect("handoff note must replace cold history");
    assert!(!handoff.content.contains("MODEL_SUMMARY_MUST_NOT_RUN"));
    assert!(!history.iter().any(|message| message.content.contains("Memory flush")));
    for event_id in persisted_ids.iter().take(3) {
        assert!(handoff.content.contains(event_id));
    }
    assert!(history.iter().any(|message| message.content == "recent assistant"));

    let events = memory
        .list_message_events_recent(&envelope.memory_principal(), 20)
        .await
        .unwrap();
    let switch_event = events
        .iter()
        .find(|event| event.event_type == "context.switch.created")
        .expect("context switch must be durably observable");
    assert!(
        switch_event
            .raw_payload_json
            .as_deref()
            .is_some_and(|payload| payload.contains("transcript_history_lookup"))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "message.created")
            .count(),
        4,
        "hard switch must not rewrite or delete durable transcript events"
    );
}

#[tokio::test]
async fn configurable_context_switch_can_roll_over_repeatedly_with_generation_lineage() {
    let temp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent("workspace-repeat-switch", "run-repeat-switch");
    let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
    let fabric = MemoryFabric::new(Arc::clone(&memory), "workspace-repeat-switch");
    let provider = ScriptedProvider::from_text_responses(vec!["unused"]);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("first old user ".repeat(120)),
        ChatMessage::assistant("first old assistant ".repeat(120)),
        ChatMessage::user("first pending user ".repeat(120)),
        ChatMessage::assistant("first retained assistant"),
    ];
    for (index, message) in history.iter().skip(1).enumerate() {
        if message.role == "user" {
            fabric
                .record_inbound_user_message(
                    envelope.message_scope(),
                    message.content.clone(),
                    Some(format!("repeat-switch-source-{index}")),
                    None,
                )
                .await
                .unwrap();
        } else {
            fabric
                .record_assistant_message(envelope.message_scope(), message.content.clone())
                .await
                .unwrap();
        }
    }
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };

    assert!(
        apply_configurable_compaction(&mut history, &provider, "model", &config, Some(&audit), "first")
            .await
            .unwrap()
    );
    let first_note = history
        .iter()
        .find(|message| is_context_handoff_message(message))
        .and_then(|message| message.content.strip_prefix(CONTEXT_HANDOFF_START))
        .and_then(|content| content.strip_suffix(CONTEXT_HANDOFF_END))
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json.trim()).ok())
        .expect("first handoff note");
    let first_handoff_id = first_note
        .get("handoff_event_id")
        .and_then(serde_json::Value::as_str)
        .unwrap()
        .to_string();
    assert_eq!(
        first_note.get("to_generation").and_then(serde_json::Value::as_u64),
        Some(1)
    );

    let second_user = ChatMessage::user("second old user ".repeat(120));
    fabric
        .record_inbound_user_message(
            envelope.message_scope(),
            second_user.content.clone(),
            Some("repeat-switch-second-user".into()),
            None,
        )
        .await
        .unwrap();
    history.push(second_user);
    let second_assistant = ChatMessage::assistant("second retained assistant");
    fabric
        .record_assistant_message(envelope.message_scope(), second_assistant.content.clone())
        .await
        .unwrap();
    history.push(second_assistant);

    assert!(
        apply_configurable_compaction(&mut history, &provider, "model", &config, Some(&audit), "second")
            .await
            .unwrap(),
        "a prior handoff must remain exact provenance for the next rollover"
    );
    let second_note = history
        .iter()
        .find(|message| is_context_handoff_message(message))
        .and_then(|message| message.content.strip_prefix(CONTEXT_HANDOFF_START))
        .and_then(|content| content.strip_suffix(CONTEXT_HANDOFF_END))
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json.trim()).ok())
        .expect("second handoff note");
    assert_eq!(
        second_note.get("from_generation").and_then(serde_json::Value::as_u64),
        Some(1)
    );
    assert_eq!(
        second_note.get("to_generation").and_then(serde_json::Value::as_u64),
        Some(2)
    );
    assert_eq!(
        second_note
            .get("parent_handoff_event_id")
            .and_then(serde_json::Value::as_str),
        Some(first_handoff_id.as_str())
    );

    let events = memory
        .list_message_events_recent(&envelope.memory_principal(), 30)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "context.switch.created")
            .count(),
        2
    );
}

#[tokio::test]
async fn configurable_context_switch_fails_closed_without_exact_provenance() {
    let provider = ScriptedProvider::from_text_responses(vec!["unused"]);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("old user ".repeat(120)),
        ChatMessage::assistant("old assistant ".repeat(120)),
        ChatMessage::user("recent user"),
    ];
    let before = history
        .iter()
        .map(|message| (message.role.clone(), message.content.clone()))
        .collect::<Vec<_>>();
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };

    let compacted = apply_configurable_compaction(&mut history, &provider, "model", &config, None, "test")
        .await
        .unwrap();

    assert!(!compacted);
    assert_eq!(
        history
            .iter()
            .map(|message| (message.role.clone(), message.content.clone()))
            .collect::<Vec<_>>(),
        before
    );
}

/// Persist exactly what the runtime persists: user turns and final assistant
/// answers. Tool results and the assistant envelope that requested them
/// never become their own `message.created` event.
async fn persist_durable_transcript(
    fabric: &MemoryFabric,
    envelope: &RuntimeEnvelope,
    history: &[ChatMessage],
) -> Vec<String> {
    let mut event_ids = Vec::new();
    for (index, message) in history.iter().enumerate().skip(1) {
        if is_tool_roundtrip_companion(message) {
            continue;
        }
        let event = if message.role == "user" {
            fabric
                .record_inbound_user_message(
                    envelope.message_scope(),
                    message.content.clone(),
                    Some(format!("tool-roundtrip-source-{index}")),
                    None,
                )
                .await
                .unwrap()
        } else {
            fabric
                .record_assistant_message(envelope.message_scope(), message.content.clone())
                .await
                .unwrap()
        };
        event_ids.push(event.event_id);
    }
    event_ids
}

/// The folded-round-trip statistics are computed from the cold window alone,
/// so they stay honest for both native and prompt-mode transcripts.
#[test]
fn folded_tool_roundtrips_counts_requests_and_names_their_tools() {
    let mut window = tool_roundtrip_history_fixture_with_tools(3, &["shell", "file_read"]);
    // Prompt mode has no assistant envelope: the `[Tool results]` block is
    // the only companion, and it is a round-trip of its own.
    window.push(ChatMessage::user(format!(
        "{} web_search output",
        crate::agent::turn_contracts::TOOL_RESULTS_PREFIX
    )));
    let folded = folded_tool_roundtrips(&window);
    assert_eq!(
        folded.count, 4,
        "three native envelopes plus one prompt-mode results block"
    );
    assert_eq!(folded.tools, vec!["shell".to_string(), "file_read".to_string()]);
    let notice = format_folded_tool_roundtrips_notice(&folded);
    assert!(
        notice.contains("4 tool round-trips (tools: shell, file_read)"),
        "notice must state the count and the tools: {notice}"
    );
    assert!(
        notice.contains("not recoverable in this window"),
        "notice must say the folded output cannot be looked up: {notice}"
    );

    let empty = folded_tool_roundtrips(&[ChatMessage::user("plain"), ChatMessage::assistant("plain")]);
    assert_eq!(empty, FoldedToolRoundtrips::default());
    assert_eq!(
        format_folded_tool_roundtrips_notice(&empty),
        "no tool round-trips were folded into this handoff"
    );
}

/// A `switch` handoff is a pointer list, and tool round-trips have no events
/// to point at. The note therefore has to say how much verbatim tool traffic
/// the window is losing and which tools produced it — without that line the
/// note reads as an exact transfer of a window that is half gone.
#[tokio::test]
async fn switch_handoff_states_how_many_tool_roundtrips_it_folded() {
    let temp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent("workspace-folded-stats", "run-folded-stats");
    let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
    let fabric = MemoryFabric::new(Arc::clone(&memory), "workspace-folded-stats");
    let provider = ScriptedProvider::from_text_responses(vec!["MODEL_SUMMARY_MUST_NOT_RUN"]);
    let mut history = tool_roundtrip_history_fixture_with_tools(10, &["shell", "file_read"]);
    assert_eq!(history.len(), 41, "fixture must cross the cold-window threshold");
    persist_durable_transcript(&fabric, &envelope, &history).await;

    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 2,
        memory_flush: true,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };
    let compacted = apply_configurable_compaction(
        &mut history,
        &provider,
        "model",
        &config,
        Some(&audit),
        "test_folded_stats",
    )
    .await
    .expect("a switch over tool round-trips must not error");
    assert!(compacted, "switch must compact the tool-bearing session");

    let handoff = history
        .iter()
        .find(|message| message.content.contains("[context_handoff]"))
        .expect("handoff note must replace the cold window");
    // The cold window is messages 1..37 — nine complete round-trips — so the
    // statistics have to report nine, not the ten the fixture built.
    assert!(
        handoff.content.contains("\"count\":9"),
        "handoff must count the folded round-trips: {}",
        handoff.content
    );
    assert!(
        handoff
            .content
            .contains("9 tool round-trips (tools: shell, file_read) were folded"),
        "handoff must name the tools whose output is gone: {}",
        handoff.content
    );
    assert!(
        handoff.content.contains("not recoverable in this window"),
        "handoff must not read as an exact transfer: {}",
        handoff.content
    );
}

#[tokio::test]
async fn context_switch_resolves_provenance_across_tool_roundtrips() {
    let temp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent("workspace-tool-switch", "run-tool-switch");
    let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
    let fabric = MemoryFabric::new(Arc::clone(&memory), "workspace-tool-switch");
    let provider = ScriptedProvider::from_text_responses(vec!["MODEL_SUMMARY_MUST_NOT_RUN"]);
    let mut history = tool_roundtrip_history_fixture(10);
    assert_eq!(history.len(), 41, "fixture must cross the cold-window threshold");
    assert_eq!(
        history.iter().filter(|message| message.role == "tool").count(),
        10,
        "fixture must carry real tool round-trips"
    );
    let persisted_ids = persist_durable_transcript(&fabric, &envelope, &history).await;
    assert_eq!(persisted_ids.len(), 20);

    // keep_recent_messages = 2 deliberately lands the retained window on the
    // tool result of the last round, so the boundary has to walk back past
    // the assistant envelope that requested it.
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 2,
        memory_flush: true,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };

    let cold_window = history.get(1..37).expect("cold window slice").to_vec();
    let provenance = resolve_compaction_event_provenance(&*memory, envelope.memory_principal(), &cold_window)
        .await
        .unwrap()
        .expect("tool round-trips must not make provenance unresolvable");
    assert_eq!(
        provenance.source_event_ids.len(),
        18,
        "every durable cold message must map to exactly one event"
    );
    assert_eq!(
        provenance.companion_message_count, 18,
        "tool envelopes and results are folded into their assistant turn"
    );

    let before_len = history.len();
    let compacted = apply_configurable_compaction(
        &mut history,
        &provider,
        "model",
        &config,
        Some(&audit),
        "test_tool_switch",
    )
    .await
    .expect("a switch over tool round-trips must not error");
    assert!(compacted, "switch must actually compact a tool-bearing session");
    assert!(
        history.len() <= 6 && history.len() < before_len / 4,
        "hot window must shrink sharply, got {} from {before_len}",
        history.len()
    );
    assert_eq!(
        first_orphan_tool_index(&history),
        None,
        "compaction must never leave a tool result without its requesting assistant message"
    );
    assert!(
        history.iter().any(|message| message.role == "tool"),
        "the most recent tool round-trip must survive intact"
    );
    assert!(
        history.iter().any(|message| message.content.contains("result 9")),
        "the newest tool result must be retained verbatim"
    );
    let handoff = history
        .iter()
        .find(|message| message.content.contains("[context_handoff]"))
        .expect("handoff note must replace the cold window");
    assert!(!handoff.content.contains("MODEL_SUMMARY_MUST_NOT_RUN"));
    assert!(
        handoff.content.contains("\"unpersisted_companion_count\":18"),
        "handoff note must disclose how many messages are covered by their assistant turn"
    );
    for event_id in persisted_ids.iter().take(18) {
        assert!(
            handoff.content.contains(event_id),
            "handoff must reference every durable cold event"
        );
    }

    let events = memory
        .list_message_events_recent(&envelope.memory_principal(), 64)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "context.switch.created")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE)
            .count(),
        0,
        "an exact switch must not report a degradation"
    );
}

#[tokio::test]
async fn context_switch_preflight_degrades_to_trim_instead_of_failing_the_turn() {
    let temp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent("workspace-tool-degrade", "run-tool-degrade");
    let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
    let provider = ScriptedProvider::from_text_responses(vec!["MODEL_SUMMARY_MUST_NOT_RUN"]);
    // No durable transcript at all: provenance cannot be exact, which used to
    // abort every turn of this session with a non-retryable error.
    let mut history = tool_roundtrip_history_fixture(10);
    let before_len = history.len();
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 400,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };

    let budget = preflight_context_budget_before_provider_call(
        &mut history,
        &provider,
        "model",
        &config,
        crate::agent::turn_context::RequestToolMeasurement::NONE,
        Some(&audit),
        "test_tool_degrade",
    )
    .await
    .expect("switch must degrade, not fail the turn");

    assert!(!budget.over_hard_limit, "the turn must continue under the hard limit");
    assert!(
        history.len() < before_len,
        "the lossy fallback must actually shrink history"
    );
    assert_eq!(
        first_orphan_tool_index(&history),
        None,
        "the lossy fallback must not leave an orphan tool result either"
    );

    let events = memory
        .list_message_events_recent(&envelope.memory_principal(), 64)
        .await
        .unwrap();
    let degraded = events
        .iter()
        .find(|event| event.event_type == CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE)
        .expect("the downgrade must be durably observable");
    assert!(
        degraded
            .raw_payload_json
            .as_deref()
            .is_some_and(|payload| payload.contains("handoff_unavailable") && payload.contains("token_aware_trim")),
        "degradation event must name the reason and the fallback"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "context.switch.created")
            .count(),
        0,
        "a degraded rollover must not leave an orphan switch event behind"
    );
}

/// A delegate or spawned sub-loop inherits the parent's
/// `source_message_event_id`, so before the nested suffix it inherited the
/// parent's `turn_key` too and the per-turn idempotency key swallowed the
/// child's degradation event: a turn in which both parent and child
/// degraded recorded one row and the child's context loss was invisible.
///
/// MUTATION GUARD: drop `into_nested_turn()` from the child below and the
/// two-event assertion goes red.
#[tokio::test]
async fn a_nested_sub_loop_records_its_own_degradation() {
    let temp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let policy = crate::security::SecurityPolicy {
        workspace_dir: temp.path().to_path_buf(),
        ..crate::security::SecurityPolicy::default()
    };
    // The one scope a parent turn hands to its delegate / spawn children.
    let scope = ScopeContext {
        policy: &policy,
        sender: "user@example.com",
        channel: "telegram",
        chat_type: "private",
        chat_id: "chat-nested-degrade",
        owner_id: None,
        topic_id: None,
        task_id: None,
        source_message_event_id: Some("inbound-msg-nested"),
        config_generation_id: None,
        config_source_revision: None,
    };

    let parent = DocumentIngestRuntime::from_scope(Arc::clone(&memory), &scope);
    let child = DocumentIngestRuntime::from_scope(Arc::clone(&memory), &scope).into_nested_turn();
    // Fixture proof: the child really is on the parent's turn lineage, which
    // is what made the two keys collide.
    assert_eq!(
        parent.source_message_event_id, child.source_message_event_id,
        "the child must inherit the parent's message lineage"
    );
    assert_ne!(parent.turn_key, child.turn_key, "the child needs its own turn key");
    assert!(
        child.turn_key.starts_with(&parent.turn_key),
        "the parent's turn must stay readable in the child's key: {}",
        child.turn_key
    );

    let principal = parent.memory_principal();
    async fn degraded_events(memory: &Arc<dyn Memory>, principal: &MemoryPrincipal) -> usize {
        memory
            .list_message_events_recent(principal, 256)
            .await
            .unwrap()
            .iter()
            .filter(|event| event.event_type == CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE)
            .count()
    }

    persist_context_switch_degradation(Some(&parent), "pre_provider_call", "no_provenance", 10, 9, 8).await;
    persist_context_switch_degradation(Some(&child), "pre_provider_call", "no_provenance", 10, 9, 8).await;
    assert_eq!(
        degraded_events(&memory, &principal).await,
        2,
        "parent and child each degraded once, so each must have its own event"
    );

    // Within one runtime the key is still constant: a child that degrades
    // again in the same turn adds nothing.
    for _ in 0..3 {
        persist_context_switch_degradation(Some(&child), "pre_provider_call", "no_provenance", 10, 9, 8).await;
    }
    assert_eq!(
        degraded_events(&memory, &principal).await,
        2,
        "repeated degradation inside one runtime must stay one event"
    );

    // And a second runtime built for the *parent* turn is still deduplicated
    // by the idempotency key, so the suffix did not weaken the parent path.
    let parent_again = DocumentIngestRuntime::from_scope(Arc::clone(&memory), &scope);
    persist_context_switch_degradation(Some(&parent_again), "pre_provider_call", "no_provenance", 10, 9, 8).await;
    assert_eq!(
        degraded_events(&memory, &principal).await,
        2,
        "a second runtime for the same parent turn must not add an event"
    );
}

#[tokio::test]
async fn a_degradation_the_backend_refused_is_retried_instead_of_claimed() {
    // The turn used to claim the write before attempting it, so a backend
    // that was briefly unavailable cost the event for the rest of the turn
    // — silently, because the failure is only a debug line. The claim now
    // follows a landed write; the idempotency key still makes the retry a
    // no-op once one has landed.
    let temp = TempDir::new().unwrap();
    let inner: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let refusing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let memory: Arc<dyn Memory> = Arc::new(RefusingEventMemory {
        inner: Arc::clone(&inner),
        refusing: Arc::clone(&refusing),
    });
    let envelope = RuntimeEnvelope::agent("workspace-degrade-retry", "run-degrade-retry")
        .with_source_message_event_id("inbound-retry-1");
    let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);

    async fn degraded_events(memory: &Arc<dyn Memory>, envelope: &RuntimeEnvelope) -> usize {
        memory
            .list_message_events_recent(&envelope.memory_principal(), 256)
            .await
            .unwrap()
            .iter()
            .filter(|event| event.event_type == CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE)
            .count()
    }

    persist_context_switch_degradation(Some(&audit), "pre_provider_call", "no_provenance", 10, 9, 8).await;
    assert_eq!(
        degraded_events(&inner, &envelope).await,
        0,
        "fixture validity: the refusing backend must really have written nothing"
    );

    // The backend comes back; the same turn must still be able to report.
    refusing.store(false, Ordering::SeqCst);
    persist_context_switch_degradation(Some(&audit), "pre_provider_call", "no_provenance", 10, 9, 8).await;
    assert_eq!(
        degraded_events(&inner, &envelope).await,
        1,
        "a turn whose first write was refused must still record the degradation"
    );

    // And it is claimed exactly once from then on.
    for _ in 0..3 {
        persist_context_switch_degradation(Some(&audit), "pre_provider_call", "no_provenance", 10, 9, 8).await;
    }
    assert_eq!(
        degraded_events(&inner, &envelope).await,
        1,
        "a landed write still claims the turn"
    );
}

/// A cold window that is nothing but one long tool round-trip chain.
///
/// Walking the boundary backwards reaches `start`, which used to mean
/// "nothing to compact": `switch` then answered `Ok(None)` on every
/// rollover and the session silently ran on the lossy trim forever. The
/// boundary must move forward to the first message that can head the hot
/// window instead.
///
/// MUTATION GUARD: delete the forward walk in
/// `compaction_boundary_without_orphan_tool_results` and both halves go red.
#[tokio::test]
async fn a_cold_window_of_only_tool_roundtrips_still_folds_into_one_handoff() {
    let temp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent("workspace-companion-window", "run-companion-window");
    let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
    let fabric = MemoryFabric::new(Arc::clone(&memory), "workspace-companion-window");
    let provider = ScriptedProvider::from_text_responses(vec!["MODEL_SUMMARY_MUST_NOT_RUN"]);

    // One durable user turn, then 20 uninterrupted tool round-trips, then
    // one durable answer: every message between them is a companion.
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user(format!("question {}", "user-context ".repeat(20))),
    ];
    for round in 0..20 {
        history.push(ChatMessage::assistant(build_native_assistant_history(
            "",
            &[ToolCall {
                id: format!("call-{round}"),
                name: "shell".to_string(),
                arguments: "{\"command\":\"ls\"}".to_string(),
            }],
            None,
        )));
        history.push(ChatMessage::tool(
            serde_json::json!({
                "tool_call_id": format!("call-{round}"),
                "content": format!("result {round} {}", "tool-output ".repeat(20)),
            })
            .to_string(),
        ));
    }
    history.push(ChatMessage::assistant(format!(
        "final answer {}",
        "assistant-context ".repeat(20)
    )));
    assert_eq!(history.len(), 43);
    assert_eq!(
        history
            .iter()
            .skip(2)
            .take(40)
            .filter(|message| is_tool_roundtrip_companion(message))
            .count(),
        40,
        "the fixture must be one uninterrupted round-trip chain"
    );

    // The unit the whole outcome turns on.
    assert_eq!(
        compaction_boundary_without_orphan_tool_results(&history, 1, 41),
        42,
        "a boundary that cannot walk back must walk forward, not give up"
    );

    let persisted = persist_durable_transcript(&fabric, &envelope, &history).await;
    assert_eq!(persisted.len(), 2, "only the two durable turns become events");

    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 2,
        memory_flush: true,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };
    let before_len = history.len();
    let compacted = apply_configurable_compaction(
        &mut history,
        &provider,
        "model",
        &config,
        Some(&audit),
        "test_companion_window",
    )
    .await
    .expect("a companion-only cold window must not error");

    assert!(compacted, "switch must still make progress on a companion-only window");
    assert!(
        history.len() < before_len / 4,
        "got {} from {before_len}",
        history.len()
    );
    assert_eq!(first_orphan_tool_index(&history), None);
    let handoff = history
        .iter()
        .find(|message| message.content.contains("[context_handoff]"))
        .expect("the window must fold into one handoff");
    assert!(
        handoff.content.contains("\"unpersisted_companion_count\":40"),
        "the handoff must disclose the folded round-trips: {}",
        handoff.content
    );
    let events = memory
        .list_message_events_recent(&envelope.memory_principal(), 64)
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE)
            .count(),
        0,
        "making progress must not be reported as a degradation"
    );
}

/// `record_tool_events = true` is a supported operator setting, and it puts
/// durable `role="tool"` events next to the round-trip messages that
/// provenance folds into their assistant turn. The two must not disagree.
#[tokio::test]
async fn switch_provenance_holds_when_tool_events_are_recorded() {
    let temp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent("workspace-tool-events", "run-tool-events");
    let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
    let recording = crate::memory::MemoryEventRecording {
        enabled: true,
        record_user_messages: true,
        record_assistant_messages: true,
        record_tool_events: true,
    };
    let fabric = MemoryFabric::new(Arc::clone(&memory), "workspace-tool-events").with_event_recording(recording);
    let provider = ScriptedProvider::from_text_responses(vec!["MODEL_SUMMARY_MUST_NOT_RUN"]);
    let mut history = tool_roundtrip_history_fixture(10);
    let persisted_ids = persist_durable_transcript(&fabric, &envelope, &history).await;
    assert_eq!(persisted_ids.len(), 20);

    // What the operator setting adds on top: one durable event per tool
    // result, interleaved with the durable turns.
    let mut tool_events = 0usize;
    for message in history.iter().filter(|message| message.role == "tool") {
        fabric
            .record_tool_event(envelope.message_scope(), message.content.clone(), None)
            .await
            .unwrap();
        tool_events += 1;
    }
    assert_eq!(tool_events, 10, "the fixture must record real tool events");
    assert_eq!(
        memory
            .list_message_events_recent(&envelope.memory_principal(), 256)
            .await
            .unwrap()
            .iter()
            .filter(|event| event.role == "tool")
            .count(),
        10,
        "fixture is useless unless the tool events are actually durable"
    );

    let cold_window = history.get(1..37).expect("cold window slice").to_vec();
    let provenance = resolve_compaction_event_provenance(&*memory, envelope.memory_principal(), &cold_window)
        .await
        .unwrap()
        .expect("recorded tool events must not make provenance unresolvable");
    assert_eq!(provenance.source_event_ids.len(), 18);
    assert_eq!(provenance.companion_message_count, 18);

    let compacted = apply_configurable_compaction(
        &mut history,
        &provider,
        "model",
        &crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Switch,
            reserve_tokens: 1,
            keep_recent_messages: 2,
            memory_flush: true,
            max_context_tokens: 50,
            max_context_tokens_explicit: true,
            os_paging: crate::config::OsPagingConfig::default(),
        },
        Some(&audit),
        "test_tool_events",
    )
    .await
    .expect("switch must still compact with tool events recorded");
    assert!(compacted);
    assert_eq!(
        memory
            .list_message_events_recent(&envelope.memory_principal(), 256)
            .await
            .unwrap()
            .iter()
            .filter(|event| event.event_type == CONTEXT_COMPACTION_DEGRADED_EVENT_TYPE)
            .count(),
        0,
        "recording tool events must not push switch into permanent degradation"
    );
}

#[tokio::test]
async fn configurable_compaction_aggressive_adds_memory_flush_note() {
    let provider = ScriptedProvider::from_text_responses(vec!["flush notes"]);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("u ".repeat(150)),
        ChatMessage::assistant("a ".repeat(150)),
        ChatMessage::user("u2 ".repeat(150)),
        ChatMessage::assistant("a2 ".repeat(150)),
    ];
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Aggressive,
        reserve_tokens: 1,
        keep_recent_messages: 1,
        memory_flush: true,
        max_context_tokens: 40,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };
    let compacted = apply_configurable_compaction(&mut history, &provider, "model", &config, None, "test")
        .await
        .unwrap();
    assert!(compacted);
    assert!(history.iter().any(|msg| msg.content.contains("[Memory flush at")));
}

#[tokio::test]
async fn configurable_compaction_records_run_and_summary_memory() {
    let tmp = TempDir::new().unwrap();
    let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent("workspace-a", "run-compact-1");
    let audit = DocumentIngestRuntime::from_envelope(mem.clone(), &envelope);
    let fabric = MemoryFabric::new(mem.clone(), "workspace-a");
    let provider = SystemSummaryProvider {
        response: "## Decisions\n- keep source anchors\n## Open TODOs\n- verify compaction\n## Critical Context\n- path /tmp/a and task run-compact-1".to_string(),
    };
    // Size off the constant so it exceeds MAX_TOOL_RESULT_INLINE_CHARS after
    // Behavior-limits Phase 1 raised it (otherwise it stays inline and no
    // document_id ref is emitted).
    let large_evidence = {
        let segment = "large output source evidence ";
        segment.repeat(MAX_TOOL_RESULT_INLINE_CHARS / segment.len() + 100)
    };
    let large_ref = compact_tool_result_for_history_with_status(
        "shell",
        &large_evidence,
        MAX_TOOL_RESULT_INLINE_CHARS,
        "persisted_document_backend",
    );
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("Remember file /tmp/a and task run-compact-1. ".repeat(120)),
        ChatMessage::assistant(format!(
            "Acknowledged source anchor requirement.\n{large_ref}\n{}",
            "padding ".repeat(120)
        )),
        ChatMessage::user("Please compact soon. ".repeat(120)),
        ChatMessage::assistant("Will preserve identifiers. ".repeat(120)),
    ];
    let mut source_event_ids = Vec::new();
    for (index, message) in history.iter().skip(1).enumerate() {
        let event = if message.role == "user" {
            fabric
                .record_inbound_user_message(
                    envelope.message_scope(),
                    message.content.clone(),
                    Some(format!("compaction-source-{index}")),
                    None,
                )
                .await
                .unwrap()
        } else {
            fabric
                .record_assistant_message(envelope.message_scope(), message.content.clone())
                .await
                .unwrap()
        };
        source_event_ids.push(event.event_id);
    }
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 1,
        keep_recent_messages: 1,
        memory_flush: false,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };

    let compacted = apply_configurable_compaction(&mut history, &provider, "model", &config, Some(&audit), "pre_turn")
        .await
        .unwrap();

    assert!(compacted);
    let conn = rusqlite::Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
    let (summary_memory_key, fidelity_status, source_events, source_range, source_refs): (
        String,
        String,
        String,
        String,
        String,
    ) = conn
        .query_row(
            "SELECT summary_memory_key, fidelity_status, source_event_ids_json,
                        source_event_range_json, source_document_refs_json
                 FROM compaction_runs
                 WHERE workspace_id = 'workspace-a'
                 ORDER BY id DESC
                 LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .unwrap();
    assert!(summary_memory_key.starts_with("compaction_summary_"));
    assert_eq!(fidelity_status, "accepted");
    let recorded_source_ids: Vec<String> = serde_json::from_str(&source_events).unwrap();
    assert_eq!(recorded_source_ids, source_event_ids[..3]);
    let covered_range: CompactionSourceEventRange = serde_json::from_str(&source_range).unwrap();
    assert_eq!(covered_range.first_event_id, source_event_ids[0]);
    assert_eq!(covered_range.last_event_id, source_event_ids[2]);
    assert_eq!(covered_range.source_event_count, 3);
    assert!(source_refs.contains("document_id"));
    assert!(source_refs.contains("tool-output:shell:"));
    assert!(source_refs.contains("persisted_document_backend"));

    let (summary_event_id, summary_causation, summary_payload): (String, String, String) = conn
        .query_row(
            "SELECT event_id, causation_event_id, raw_payload_json
                 FROM message_events
                 WHERE event_type = 'compaction.summary.created'
                 ORDER BY id DESC
                 LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(summary_causation, source_event_ids[2]);
    let summary_payload: serde_json::Value = serde_json::from_str(&summary_payload).unwrap();
    assert_eq!(
        summary_payload
            .get("covered_event_range")
            .and_then(|range| range.get("last_event_id"))
            .and_then(serde_json::Value::as_str),
        Some(source_event_ids[2].as_str())
    );
    let compaction_payload: String = conn
        .query_row(
            "SELECT payload_json FROM compaction_runs WHERE workspace_id = 'workspace-a' ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let compaction_payload: serde_json::Value = serde_json::from_str(&compaction_payload).unwrap();
    assert_eq!(
        compaction_payload
            .get("summary_message_event_id")
            .and_then(serde_json::Value::as_str),
        Some(summary_event_id.as_str())
    );

    let stored_summary_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE key = ?1 AND source = 'compaction_summary'",
            [summary_memory_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_summary_count, 1);
}

#[tokio::test]
async fn compaction_provenance_refuses_ambiguous_repeated_event_sequences() {
    let tmp = TempDir::new().unwrap();
    let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent("workspace-a", "ambiguous-compaction");
    let fabric = MemoryFabric::new(mem.clone(), "workspace-a");
    for index in 0..2 {
        fabric
            .record_inbound_user_message(
                envelope.message_scope(),
                "same repeated turn",
                Some(format!("ambiguous-{index}")),
                None,
            )
            .await
            .unwrap();
    }

    let provenance = resolve_compaction_event_provenance(
        mem.as_ref(),
        envelope.memory_principal(),
        &[ChatMessage::user("same repeated turn")],
    )
    .await
    .unwrap();
    assert!(
        provenance.is_none(),
        "ambiguous matches must not select arbitrary event IDs"
    );
}

/// D-054: turn commits persist replies redacted but never capped, so a long
/// reply that carried a secret matches its durable event only in the
/// redacted-uncapped form; events chat wrote before D-054 (redacted and
/// capped) and unredacted ones still match, and a different text does not.
#[test]
fn compaction_provenance_matches_a_redacted_uncapped_reply_event() {
    let long_reply = format!("key AKIAIOSFODNN7EXAMPLE then {}", "x".repeat(20 * 1024));
    let message = ChatMessage::assistant(long_reply.clone());
    let event = |content: String| {
        let mut event: MessageEvent = serde_json::from_value(serde_json::json!({
            "id": 0,
            "event_id": "evt-compaction-redacted",
            "workspace_id": "workspace-a",
            "source": "chat",
            "role": "assistant",
            "content": "",
            "visibility": "workspace",
            "created_at": "2026-03-05T00:00:00Z",
            "updated_at": "2026-03-05T00:00:00Z"
        }))
        .expect("test: a minimal message event");
        event.content = content;
        event
    };
    let redacted = crate::security::redaction::redact_secrets(&long_reply);
    assert!(redacted.len() > 10 * 1024, "the redacted reply is not capped");
    assert!(source_message_matches_event(&message, &event(redacted)));
    assert!(source_message_matches_event(
        &message,
        &event(crate::chat::sanitize::sanitize_for_persistence(&long_reply))
    ));
    assert!(source_message_matches_event(&message, &event(long_reply)));
    assert!(!source_message_matches_event(
        &message,
        &event("another reply".to_string())
    ));
}
