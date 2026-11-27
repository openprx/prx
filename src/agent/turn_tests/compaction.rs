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
use crate::config::agent_name::AgentName;
use crate::memory::{
    AgentPartition, CompactionSourceEventRange, DocumentIngestInput, DocumentRecord, Memory, MemoryCategory,
    MemoryEntry, MemoryFabric, MemoryPrincipal, MessageEvent, MessageEventInput, PartitionKey, SqliteMemory,
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
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 100,
        max_context_tokens: 500,
        ..crate::config::AgentCompactionConfig::default()
    };
    assert_eq!(compaction_trigger_limit(&config), Some(400));
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
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
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
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
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

    // The second rollover is the next admitted turn: its automatic switch budget
    // is fresh, so it builds on its own runtime rather than the spent one.
    let next_turn_audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
    assert!(
        apply_configurable_compaction(
            &mut history,
            &provider,
            "model",
            &config,
            Some(&next_turn_audit),
            "second"
        )
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
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
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
    let companions = shape_companions(&window);
    let folded = folded_tool_roundtrips(&window, RoundCompanions::Marked(&companions));
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

    let plain = [ChatMessage::user("plain"), ChatMessage::assistant("plain")];
    let empty = folded_tool_roundtrips(&plain, RoundCompanions::Marked(&shape_companions(&plain)));
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
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
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
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
    };

    let cold_window = history.get(1..37).expect("cold window slice").to_vec();
    let companions = shape_companions(&cold_window);
    let provenance = resolve_compaction_event_provenance(
        &*memory,
        envelope.memory_principal(),
        &cold_window,
        RoundCompanions::Marked(&companions),
    )
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
        max_context_tokens: 400,
        max_context_tokens_explicit: true,
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
        session_key: None,
        owner_id: None,
        owner_isolated: false,
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
        compaction_boundary_without_orphan_tool_results(&shape_companions(&history), 1, 41),
        42,
        "a boundary that cannot walk back must walk forward, not give up"
    );

    let persisted = persist_durable_transcript(&fabric, &envelope, &history).await;
    assert_eq!(persisted.len(), 2, "only the two durable turns become events");

    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 2,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
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
    let companions = shape_companions(&cold_window);
    let provenance = resolve_compaction_event_provenance(
        &*memory,
        envelope.memory_principal(),
        &cold_window,
        RoundCompanions::Marked(&companions),
    )
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
            max_context_tokens: 50,
            max_context_tokens_explicit: true,
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
        RoundCompanions::Acknowledged,
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

fn numbered_history(len: usize) -> Vec<ChatMessage> {
    let mut history = vec![ChatMessage::system("sys")];
    history.extend((1..len).map(|index| {
        if index % 2 == 1 {
            ChatMessage::user(format!("question {index}"))
        } else {
            ChatMessage::assistant(format!("answer {index}"))
        }
    }));
    history
}

fn patch_over(history: &[ChatMessage], start: usize, end: usize) -> CompactionPatch {
    CompactionPatch {
        replacement: vec![ChatMessage::assistant("handoff")],
        append_after: Vec::new(),
        guard: compaction_patch_guard_for(history, start, end).expect("test: guard over an in-range span"),
    }
}

fn contents(history: &[ChatMessage]) -> Vec<(String, String)> {
    history
        .iter()
        .map(|message| (message.role.clone(), message.content.clone()))
        .collect()
}

#[test]
fn apply_compaction_patch_exact_rejects_out_of_range_patch_without_mutation() {
    // The guard describes a 50-message source; the history is the 17-message
    // provider view of the regression.
    let source = numbered_history(50);
    let patch = patch_over(&source, 1, 40);
    let mut provider_view = numbered_history(17);
    let before = contents(&provider_view);

    let result = apply_compaction_patch_exact(&mut provider_view, &patch);

    assert_eq!(
        result,
        Err(crate::agent::turn_contracts::CompactionPatchError::RangeOutOfBounds {
            start: 1,
            end: 40,
            len: 17
        }),
        "a range past the history end must be a typed out-of-range error, not a splice panic"
    );
    assert_eq!(
        contents(&provider_view),
        before,
        "a refused patch must leave the history unchanged"
    );
}

#[test]
fn apply_compaction_patch_exact_rejects_guard_mismatch_without_mutation() {
    let source = numbered_history(20);
    let patch = patch_over(&source, 1, 10);
    let mut other = numbered_history(20);
    if let Some(message) = other.get_mut(3) {
        message.content = "edited after the guard was taken".to_owned();
    }
    let before = contents(&other);

    let result = apply_compaction_patch_exact(&mut other, &patch);

    assert_eq!(
        result,
        Err(crate::agent::turn_contracts::CompactionPatchError::GuardMismatch {
            expected_len: 20,
            actual_len: 20
        }),
        "an in-range history the guard does not describe must be a typed guard mismatch"
    );
    assert_eq!(
        contents(&other),
        before,
        "a refused patch must leave the history unchanged"
    );
}

#[test]
fn apply_compaction_patch_exact_replaces_exactly_the_guarded_range() {
    let source = numbered_history(12);
    let mut patch = patch_over(&source, 1, 9);
    patch.append_after = vec![ChatMessage::user("refresh")];
    let mut history = source.clone();

    apply_compaction_patch_exact(&mut history, &patch).expect("test: the guard describes its own source");

    let mut expected = vec![
        source[0].clone(),
        ChatMessage::assistant("handoff"),
        ChatMessage::user("refresh"),
    ];
    expected.extend(source[9..].iter().cloned());
    assert_eq!(
        contents(&history),
        contents(&expected),
        "the guarded range must be replaced and the tail kept verbatim"
    );
}

/// Bound for the preflight capture tests (in-process work only).
const PREFLIGHT_CAPTURE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// The per-provider preflight hands its caller the durable part of what it
/// did. A summary is built from the turn's canonical history, so the turns an
/// earlier trim of this turn left out of the provider view are part of its
/// source; a degraded switch that only trims returns nothing (a projection of
/// this request) and leaves the canonical history whole.
#[tokio::test]
async fn the_preflight_builds_its_durable_compaction_from_the_canonical_history_and_nothing_for_a_trim() {
    tokio::time::timeout(PREFLIGHT_CAPTURE_DEADLINE, async {
        let summary_provider =
            ScriptedProvider::from_text_responses(vec!["## Decisions\n- d\n## Open TODOs\n- t\n## Constraints\n- c"]);
        let original = tool_roundtrip_history_fixture(10);
        let summary_config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Switch,
            reserve_tokens: 1,
            keep_recent_messages: 2,
            max_context_tokens: 400,
            max_context_tokens_explicit: true,
        };
        // An earlier trim of this turn took the two oldest rounds out of the view.
        let mut view = original.clone();
        let mut turn_history = crate::agent::turn_history::TurnHistory::new(&view, None, false);
        turn_history.reshape(&mut view, |view| {
            view.drain(1..9);
        });
        let compaction = crate::agent::context_preflight::preflight_durable_compaction(
            &view,
            &mut turn_history,
            &summary_provider,
            "model",
            &summary_config,
            crate::agent::turn_context::RequestToolMeasurement::NONE,
            None,
            "test_capture_summary",
        )
        .await
        .expect("the summary preflight runs");

        let temp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).unwrap());
        let envelope = RuntimeEnvelope::agent("workspace-capture-trim", "run-capture-trim");
        let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &envelope);
        let unused_provider = ScriptedProvider::from_text_responses(vec!["MODEL_SUMMARY_MUST_NOT_RUN"]);
        let switch_config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Switch,
            ..summary_config.clone()
        };
        let mut trimmed = tool_roundtrip_history_fixture(10);
        let trimmed_len = trimmed.len();
        let mut trim_history = crate::agent::turn_history::TurnHistory::new(&trimmed, None, false);
        let trim_only = crate::agent::context_preflight::preflight_durable_compaction(
            &trimmed,
            &mut trim_history,
            &unused_provider,
            "model",
            &switch_config,
            crate::agent::turn_context::RequestToolMeasurement::NONE,
            Some(&audit),
            "test_capture_trim",
        )
        .await
        .expect("the degraded switch preflight runs");
        crate::agent::context_preflight::preflight_trim_projection(
            &mut trimmed,
            &mut trim_history,
            &switch_config,
            &trim_only,
            Some(&audit),
            "test_capture_trim",
        )
        .await;

        let before = compaction
            .source
            .as_ref()
            .expect("an applied summary is returned as a durable compaction");
        assert!(
            crate::agent::turn_history::same_messages(before.messages(), &original),
            "the durable compaction's source is the canonical history, the trimmed rounds included"
        );
        assert!(
            turn_history
                .canonical(&view)
                .iter()
                .any(|message| message.content.contains("[Context compacted at")),
            "the compacted canonical history carries the summary"
        );
        assert!(
            trim_only.source.is_none(),
            "a trim without a handoff is not a durable compaction"
        );
        assert!(trimmed.len() < trimmed_len, "the degraded switch did trim the view");
        assert!(
            crate::agent::turn_history::same_messages(trim_history.canonical_mut(&trimmed).messages(), &original),
            "the trim never reached the canonical history"
        );
    })
    .await
    .expect("the preflight capture test finishes within PREFLIGHT_CAPTURE_DEADLINE");
}

/// The companion flags of a fixture window that carries no recorded
/// provenance (as a caller without a session): read by the round-trip shape.
fn shape_companions(messages: &[ChatMessage]) -> Vec<bool> {
    crate::agent::turn_history::CanonicalHistory::from_test_messages(messages.to_vec()).tool_round_companions()
}

// ---------------------------------------------------------------------------
// Exact switch provenance when store order differs from transcript order
// (REQ-BRANCH-PROVENANCE-LOSSY-TRIM, T1-T5e).
// ---------------------------------------------------------------------------

/// Bound for one provenance test, its real SQLite work and its cleanup.
const PROVENANCE_TEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
const PROVENANCE_WORKSPACE: &str = "workspace-provenance";
/// Most pages read while counting stored switch rows.
const SWITCH_ROW_PAGE_BOUND: usize = 64;

/// One chat session on a real SQLite store, written through the real fabric.
struct ProvenanceStore {
    _temp: TempDir,
    memory: Arc<dyn Memory>,
    fabric: MemoryFabric,
    session: RuntimeEnvelope,
    session_id: String,
}

impl ProvenanceStore {
    fn new(session_id: &str) -> Self {
        let temp = TempDir::new().expect("test: tempdir");
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: sqlite"));
        let fabric = MemoryFabric::new(Arc::clone(&memory), PROVENANCE_WORKSPACE);
        let session = RuntimeEnvelope::chat_canonical(
            PROVENANCE_WORKSPACE,
            session_id,
            crate::memory::MemoryVisibility::Workspace,
        );
        Self {
            _temp: temp,
            memory,
            fabric,
            session,
            session_id: session_id.to_string(),
        }
    }

    /// The chat envelope of one turn: `Some(run)` is the turn's run id (the
    /// chat contract D8-2); `None` writes a row without a run id.
    fn turn(&self, run: Option<&str>) -> RuntimeEnvelope {
        run.map_or_else(|| self.session.clone(), |run| self.session.clone().with_run_id(run))
    }

    async fn user(&self, run: Option<&str>, text: &str) -> MessageEvent {
        self.fabric
            .record_inbound_user_message(
                self.turn(run).message_scope(),
                text,
                Some(format!("provenance-input-{}", uuid::Uuid::new_v4())),
                None,
            )
            .await
            .expect("test: record user row")
    }

    async fn assistant(&self, run: Option<&str>, text: &str) -> MessageEvent {
        self.fabric
            .record_assistant_message(self.turn(run).message_scope(), text)
            .await
            .expect("test: record assistant row")
    }

    /// `pairs` unrelated user/assistant pairs, each with its own run.
    async fn unrelated_rows(&self, label: &str, pairs: usize) {
        for pair in 0..pairs {
            let run = format!("{label}-run-{pair}");
            self.user(Some(run.as_str()), &format!("{label} unrelated question {pair}"))
                .await;
            self.assistant(Some(run.as_str()), &format!("{label} unrelated answer {pair}"))
                .await;
        }
    }

    fn audit(&self) -> DocumentIngestRuntime {
        DocumentIngestRuntime::from_envelope(Arc::clone(&self.memory), &self.session)
    }

    /// The real switch builder over `history`, keeping `keep_recent` messages.
    async fn build(&self, history: &[ChatMessage], keep_recent: usize) -> Option<CompactionPatch> {
        let provider = ScriptedProvider::from_text_responses(vec!["MODEL_SUMMARY_MUST_NOT_RUN"]);
        let config = crate::config::AgentCompactionConfig {
            mode: crate::config::AgentCompactionMode::Switch,
            reserve_tokens: 1,
            keep_recent_messages: keep_recent,
            max_context_tokens: 50,
            max_context_tokens_explicit: true,
        };
        let audit = self.audit();
        build_configurable_compaction_patch(history, &provider, "model", &config, Some(&audit), "test_provenance")
            .await
            .expect("test: the switch builder runs")
    }

    async fn detailed(&self, cold: &[ChatMessage]) -> ProvenanceResolution {
        resolve_compaction_event_provenance_detailed(
            self.memory.as_ref(),
            self.session.memory_principal(),
            cold,
            RoundCompanions::Acknowledged,
        )
        .await
        .expect("test: the provenance read succeeds")
    }

    /// Every stored `context.switch.created` row of the store.
    async fn switch_rows(&self) -> Vec<MessageEvent> {
        let principal = self.session.memory_principal();
        let mut cursor = 0_i64;
        let mut rows = Vec::new();
        for _ in 0..SWITCH_ROW_PAGE_BOUND {
            let page = self
                .memory
                .list_message_events_since(&principal, cursor, 500)
                .await
                .expect("test: list stored rows");
            let Some(last) = page.last() else {
                return rows;
            };
            cursor = last.id;
            rows.extend(
                page.into_iter()
                    .filter(|event| event.event_type == "context.switch.created"),
            );
        }
        panic!("test: the store has more than {SWITCH_ROW_PAGE_BOUND} pages of rows");
    }

    /// Read the note's rows back through the real lookup tool.
    async fn lookup(&self, note: &serde_json::Value) -> Vec<String> {
        use crate::tools::traits::Tool as _;
        let result =
            crate::tools::transcript_history_lookup::TranscriptHistoryLookupTool::new(Arc::clone(&self.memory))
                .execute(serde_json::json!({
                    "handoff_event_id": note.get("handoff_event_id"),
                    "_zc_scope_trusted": true,
                    "_zc_scope": {
                        "workspace_id": self.session.workspace_id,
                        "session_key": self.session.session_key,
                        "channel": self.session.channel,
                        "sender": self.session.sender,
                        "owner_id": self.session.resolved_owner_id()
                    }
                }))
                .await
                .expect("test: the lookup runs");
        assert!(result.success, "the lookup succeeds: {:?}", result.error);
        let output: serde_json::Value = serde_json::from_str(&result.output).expect("test: lookup output JSON");
        output
            .get("events")
            .and_then(serde_json::Value::as_array)
            .expect("test: lookup events")
            .iter()
            .filter_map(|event| event.get("content").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect()
    }
}

/// A transcript text long enough that a few messages cross the 50-token window.
fn padded(label: &str) -> String {
    format!("{label} {}", "padding ".repeat(20))
}

/// The validated note of a switch patch.
fn switch_note(patch: &CompactionPatch) -> serde_json::Value {
    let handoff = patch.replacement.first().expect("test: the patch carries the handoff");
    validated_context_handoff_note(&handoff.content).expect("test: the handoff note validates")
}

fn note_event_ids(note: &serde_json::Value) -> Vec<String> {
    note.pointer("/lookup/arguments/event_ids")
        .and_then(serde_json::Value::as_array)
        .expect("test: lookup event ids")
        .iter()
        .map(|id| id.as_str().expect("test: string event id").to_string())
        .collect()
}

fn ids_of(events: &[&MessageEvent]) -> Vec<String> {
    events.iter().map(|event| event.event_id.clone()).collect()
}

/// Assert `patch` is an exact switch whose note references `expected` in
/// history order and whose lookup returns their texts.
async fn assert_exact_switch(store: &ProvenanceStore, patch: Option<CompactionPatch>, expected: &[&MessageEvent]) {
    let patch = patch.expect("test: an exact cold window yields a switch patch");
    let note = switch_note(&patch);
    assert_eq!(
        note.get("provenance_status").and_then(serde_json::Value::as_str),
        Some("exact"),
        "the handoff records exact provenance: {note}"
    );
    assert_eq!(
        note_event_ids(&note),
        ids_of(expected),
        "the handoff references the cold rows in history order"
    );
    let contents = store.lookup(&note).await;
    for event in expected {
        assert!(
            contents.contains(&event.content),
            "the lookup returns the referenced row `{}`: {contents:?}",
            event.content
        );
    }
    assert_eq!(
        store.switch_rows().await.len(),
        1,
        "exactly one switch row is persisted"
    );
}

/// Assert the cold window resolves to `gap`, the builder refuses the switch
/// and no switch row is persisted.
async fn assert_refused_switch(
    store: &ProvenanceStore,
    history: &[ChatMessage],
    keep_recent: usize,
    gap: ProvenanceGap,
) {
    let cold = history
        .get(1..history.len() - keep_recent)
        .expect("test: the cold window is in range");
    assert_eq!(
        store.detailed(cold).await,
        ProvenanceResolution::Unavailable(gap),
        "the detailed resolver names the gap"
    );
    assert!(
        store.build(history, keep_recent).await.is_none(),
        "the builder refuses a switch without exact provenance"
    );
    assert!(
        store.switch_rows().await.is_empty(),
        "no context.switch.created row is persisted"
    );
}

#[tokio::test]
async fn switch_provenance_is_exact_when_a_queued_input_precedes_the_previous_reply() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        let store = ProvenanceStore::new("t1-queued-input");
        let u1 = store.user(Some("r1"), &padded("first question")).await;
        let u2 = store.user(Some("r2"), &padded("queued question")).await;
        let a1 = store.assistant(Some("r1"), &padded("first answer")).await;
        let a2 = store.assistant(Some("r2"), &padded("queued answer")).await;
        let u3 = store.user(Some("r3"), &padded("third question")).await;
        let a3 = store.assistant(Some("r3"), &padded("third answer")).await;
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(u1.content.clone()),
            ChatMessage::assistant(a1.content.clone()),
            ChatMessage::user(u2.content.clone()),
            ChatMessage::assistant(a2.content.clone()),
            ChatMessage::user(u3.content.clone()),
            ChatMessage::assistant(a3.content.clone()),
        ];

        let patch = store.build(&history, 2).await;

        assert_exact_switch(&store, patch, &[&u1, &a1, &u2, &a2]).await;
    })
    .await
    .expect("switch_provenance_is_exact_when_a_queued_input_precedes_the_previous_reply exceeded its deadline");
}

#[tokio::test]
async fn switch_provenance_ignores_child_rows_under_the_legacy_key() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        let store = ProvenanceStore::new("t2-child-rows");
        let u1 = store.user(Some("r1"), &padded("parent question")).await;
        let child =
            RuntimeEnvelope::sessions_spawn(PROVENANCE_WORKSPACE, format!("chat:{}", store.session_id), "child-run")
                .with_parent_run_id("r1");
        let mut child_ids = Vec::new();
        // One child input repeats the parent's text: it still never enters.
        for text in [u1.content.clone(), padded("child task")] {
            let event = store
                .fabric
                .record_inbound_user_message(
                    child.message_scope(),
                    text,
                    Some(format!("provenance-child-{}", uuid::Uuid::new_v4())),
                    None,
                )
                .await
                .expect("test: record child user row");
            child_ids.push(event.event_id);
        }
        let child_reply = store
            .fabric
            .record_assistant_message(child.message_scope(), padded("child result"))
            .await
            .expect("test: record child assistant row");
        child_ids.push(child_reply.event_id);
        let a1 = store.assistant(Some("r1"), &padded("parent answer")).await;
        let u2 = store.user(Some("r2"), &padded("second question")).await;
        let a2 = store.assistant(Some("r2"), &padded("second answer")).await;
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(u1.content.clone()),
            ChatMessage::assistant(a1.content.clone()),
            ChatMessage::user(u2.content.clone()),
            ChatMessage::assistant(a2.content.clone()),
            ChatMessage::user(padded("tail question")),
        ];

        let patch = store.build(&history, 1).await;

        let note = switch_note(patch.as_ref().expect("test: a switch patch"));
        assert!(
            note_event_ids(&note).iter().all(|id| !child_ids.contains(id)),
            "no child row under the legacy key is referenced"
        );
        assert_exact_switch(&store, patch, &[&u1, &a1, &u2, &a2]).await;
    })
    .await
    .expect("switch_provenance_ignores_child_rows_under_the_legacy_key exceeded its deadline");
}

#[tokio::test]
async fn switch_provenance_skips_the_input_row_of_a_failed_turn() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        let store = ProvenanceStore::new("t3-failed-turn");
        let u1 = store.user(Some("r1"), &padded("first question")).await;
        let a1 = store.assistant(Some("r1"), &padded("first answer")).await;
        store
            .user(Some("r-failed"), &padded("a request whose turn failed"))
            .await;
        let u2 = store.user(Some("r2"), &padded("second question")).await;
        let a2 = store.assistant(Some("r2"), &padded("second answer")).await;
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(u1.content.clone()),
            ChatMessage::assistant(a1.content.clone()),
            ChatMessage::user(u2.content.clone()),
            ChatMessage::assistant(a2.content.clone()),
            ChatMessage::user(padded("tail question")),
        ];

        let patch = store.build(&history, 1).await;

        assert_exact_switch(&store, patch, &[&u1, &a1, &u2, &a2]).await;
    })
    .await
    .expect("switch_provenance_skips_the_input_row_of_a_failed_turn exceeded its deadline");
}

#[tokio::test]
async fn switch_provenance_rejects_a_contiguous_window_that_contradicts_run_ids() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        let store = ProvenanceStore::new("t3a-run-ids");
        let question = padded("repeated question");
        let user = store.user(Some("r1"), &question).await;
        let failed = store.user(Some("r2"), &question).await;
        let reply = store.assistant(Some("r1"), &padded("the answer")).await;
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(question.clone()),
            ChatMessage::assistant(reply.content.clone()),
            ChatMessage::user(padded("tail question")),
        ];

        let patch = store.build(&history, 1).await;

        let note = switch_note(patch.as_ref().expect("test: a switch patch"));
        assert!(
            !note_event_ids(&note).contains(&failed.event_id),
            "the window's failed duplicate input is not referenced"
        );
        assert_exact_switch(&store, patch, &[&user, &reply]).await;
    })
    .await
    .expect("switch_provenance_rejects_a_contiguous_window_that_contradicts_run_ids exceeded its deadline");
}

#[tokio::test]
async fn switch_provenance_refuses_a_window_justified_only_by_adjacency() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        let store = ProvenanceStore::new("t3b-adjacency");
        let question = padded("repeated question");
        store.user(Some("r1"), &question).await;
        store.user(None, &question).await;
        let reply = store.assistant(Some("r1"), &padded("the answer")).await;
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(question.clone()),
            ChatMessage::assistant(reply.content.clone()),
            ChatMessage::user(padded("tail question")),
        ];

        assert_refused_switch(&store, &history, 1, ProvenanceGap::TurnAmbiguous).await;
    })
    .await
    .expect("switch_provenance_refuses_a_window_justified_only_by_adjacency exceeded its deadline");
}

#[tokio::test]
async fn switch_provenance_orders_identical_turns_by_commit_order() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        let store = ProvenanceStore::new("t4-identical-turns");
        let question = padded("Run echo t5-shell-deny");
        let answer = padded("t5-shell-deny");
        let first_user = store.user(Some("r1"), &question).await;
        let queued_user = store.user(Some("r2"), &padded("queued question")).await;
        let first_reply = store.assistant(Some("r1"), &answer).await;
        let queued_reply = store.assistant(Some("r2"), &padded("queued answer")).await;
        let again_user = store.user(Some("r3"), &question).await;
        let again_reply = store.assistant(Some("r3"), &answer).await;
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(question.clone()),
            ChatMessage::assistant(answer.clone()),
            ChatMessage::user(queued_user.content.clone()),
            ChatMessage::assistant(queued_reply.content.clone()),
            ChatMessage::user(question.clone()),
            ChatMessage::assistant(answer.clone()),
            ChatMessage::user(padded("tail question")),
        ];

        let patch = store.build(&history, 1).await;

        assert_exact_switch(
            &store,
            patch,
            &[
                &first_user,
                &first_reply,
                &queued_user,
                &queued_reply,
                &again_user,
                &again_reply,
            ],
        )
        .await;
    })
    .await
    .expect("switch_provenance_orders_identical_turns_by_commit_order exceeded its deadline");
}

#[tokio::test]
async fn switch_provenance_refuses_a_trailing_input_it_cannot_tell_from_a_failed_duplicate() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        let store = ProvenanceStore::new("t5-trailing-input");
        let u1 = store.user(Some("r1"), &padded("first question")).await;
        let a1 = store.assistant(Some("r1"), &padded("first answer")).await;
        store.user(Some("r2"), "n").await;
        store.user(Some("r-orphan"), "n").await;
        let reply = store.assistant(Some("r2"), &padded("declined")).await;
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(u1.content.clone()),
            ChatMessage::assistant(a1.content.clone()),
            ChatMessage::user("n"),
            ChatMessage::assistant(reply.content.clone()),
        ];

        assert_refused_switch(&store, &history, 1, ProvenanceGap::TurnAmbiguous).await;
    })
    .await
    .expect("switch_provenance_refuses_a_trailing_input_it_cannot_tell_from_a_failed_duplicate exceeded its deadline");
}

#[tokio::test]
async fn switch_provenance_never_lets_a_missing_run_id_eliminate_a_competitor() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        let store = ProvenanceStore::new("t5a-missing-run");
        let question = padded("repeated question");
        let answer = padded("repeated answer");
        store.user(Some("r1"), &question).await;
        store.user(None, &question).await;
        store.user(Some("rx"), &padded("unrelated question")).await;
        store.assistant(Some("rx"), &padded("unrelated answer")).await;
        store.assistant(None, &answer).await;
        store.assistant(Some("r1"), &answer).await;
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(question.clone()),
            ChatMessage::assistant(answer.clone()),
            ChatMessage::user(padded("tail question")),
        ];

        assert_refused_switch(&store, &history, 1, ProvenanceGap::TurnAmbiguous).await;
    })
    .await
    .expect("switch_provenance_never_lets_a_missing_run_id_eliminate_a_competitor exceeded its deadline");
}

/// A pool of `pairs` user/assistant rows in store order; `text` names the
/// pair's text and `run` its run id.
fn pool_pairs(pairs: usize, text: impl Fn(usize) -> usize, run: impl Fn(usize) -> Option<String>) -> Vec<PoolRow> {
    let mut rows = Vec::new();
    for pair in 0..pairs {
        for (offset, role) in [(0, PoolRole::User), (1, PoolRole::Assistant)] {
            let rank = pair * 2 + offset;
            rows.push(PoolRow {
                id: i64::try_from(rank + 1).expect("test: small row id"),
                event_id: format!("row-{rank}"),
                role,
                run_id: run(pair),
                // Message 2k is the user and 2k+1 the assistant of text k.
                matches: vec![text(pair) * 2 + offset],
                store_rank: rank,
            });
        }
    }
    rows
}

#[test]
fn switch_provenance_reports_an_exhausted_search_as_ambiguous() {
    const PAIRS: usize = 30;
    const SMALL_BUDGET: usize = 40;
    // Identical run-less pairs: every user row matches every user message.
    let identical_messages = (0..PAIRS)
        .flat_map(|_| {
            [
                ChatMessage::user("same question"),
                ChatMessage::assistant("same answer"),
            ]
        })
        .collect::<Vec<_>>();
    let identical_segment = identical_messages.iter().collect::<Vec<_>>();
    let mut identical_pool = pool_pairs(PAIRS, |_| 0, |_| None);
    for row in &mut identical_pool {
        let offset = usize::from(row.role == PoolRole::Assistant);
        row.matches = (0..PAIRS).map(|pair| pair * 2 + offset).collect();
    }
    assert_eq!(
        match_segment_within(&identical_segment, 0, &identical_pool, &HashSet::new(), SMALL_BUDGET)
            .map(|rows| rows.len()),
        Err(ProvenanceGap::TurnAmbiguous),
        "a search that runs out of budget is ambiguous"
    );

    // Unique pairs: one admissible assignment, found within the full budget
    // but not within the small one.
    let unique_messages = (0..PAIRS)
        .flat_map(|pair| {
            [
                ChatMessage::user(format!("question {pair}")),
                ChatMessage::assistant(format!("answer {pair}")),
            ]
        })
        .collect::<Vec<_>>();
    let unique_segment = unique_messages.iter().collect::<Vec<_>>();
    let unique_pool = pool_pairs(PAIRS, |pair| pair, |_| None);
    assert_eq!(
        match_segment_within(&unique_segment, 0, &unique_pool, &HashSet::new(), SMALL_BUDGET).map(|rows| rows.len()),
        Err(ProvenanceGap::TurnAmbiguous),
        "an exhausted budget never yields a guess, even when one assignment exists"
    );
    assert_eq!(
        match_segment(&unique_segment, 0, &unique_pool, &HashSet::new())
            .map(|rows| rows.iter().map(|row| row.id).collect::<Vec<_>>()),
        Ok((1..=i64::try_from(PAIRS * 2).expect("test: small row id")).collect::<Vec<_>>()),
        "the full budget proves the unique assignment"
    );
}

#[tokio::test]
async fn switch_provenance_refuses_a_cold_pair_repeated_in_the_retained_tail_beyond_the_first_page() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        let store = ProvenanceStore::new("t5c-retained-repeat");
        let question = padded("repeated question");
        let answer = padded("repeated answer");
        let cold_user = store.user(Some("r1"), &question).await;
        let cold_reply = store.assistant(Some("r1"), &answer).await;
        store.unrelated_rows("before", 249).await;
        let retained_user = store.user(Some("r2"), &question).await;
        let retained_reply = store.assistant(Some("r2"), &answer).await;
        store.unrelated_rows("after", 249).await;
        assert_eq!(
            (retained_user.id - cold_user.id, retained_reply.id - cold_reply.id),
            (500, 500),
            "the retained pair lies 500 rows after the cold pair"
        );
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(question.clone()),
            ChatMessage::assistant(answer.clone()),
            ChatMessage::user(question.clone()),
            ChatMessage::assistant(answer.clone()),
            ChatMessage::user(padded("tail question")),
        ];

        assert_refused_switch(&store, &history, 3, ProvenanceGap::TurnAmbiguous).await;
        for row in store.switch_rows().await {
            assert!(
                !row.content.contains(&retained_user.event_id) && !row.content.contains(&retained_reply.event_id),
                "the retained rows never appear in a persisted note"
            );
        }
    })
    .await
    .expect(
        "switch_provenance_refuses_a_cold_pair_repeated_in_the_retained_tail_beyond_the_first_page exceeded its deadline",
    );
}

/// A store that forwards the newest-page read of `load_recent_session_context`
/// but has the trait's fail-fast default for `load_session_context_before`,
/// through which the provenance pool reads every page.
struct FirstPageOnlyMemory {
    inner: Arc<dyn Memory>,
}

#[async_trait]
impl Memory for FirstPageOnlyMemory {
    fn name(&self) -> &str {
        "first-page-only"
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
    async fn load_recent_session_context(
        &self,
        query: crate::memory::SessionContextQuery,
    ) -> anyhow::Result<Vec<MessageEvent>> {
        self.inner.load_recent_session_context(query).await
    }
}

#[tokio::test]
async fn switch_provenance_pages_past_the_first_window_and_resolves_exact() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        let store = ProvenanceStore::new("t5d-paging");
        let cold_user = store.user(Some("r1"), &padded("the oldest question")).await;
        let cold_reply = store.assistant(Some("r1"), &padded("the oldest answer")).await;
        store.unrelated_rows("later", 499).await;
        store.user(Some("r-tail"), &padded("tail question")).await;
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(cold_user.content.clone()),
            ChatMessage::assistant(cold_reply.content.clone()),
            ChatMessage::user(padded("tail question")),
        ];
        let cold = history.get(1..3).expect("test: cold window");

        let paging_only = FirstPageOnlyMemory {
            inner: Arc::clone(&store.memory),
        };
        assert_eq!(
            resolve_compaction_event_provenance_detailed(
                &paging_only,
                store.session.memory_principal(),
                cold,
                RoundCompanions::Acknowledged,
            )
            .await
            .expect("test: the first page reads"),
            ProvenanceResolution::Unavailable(ProvenanceGap::TurnAmbiguous),
            "a backend that cannot page leaves the pool unproven"
        );

        let patch = store.build(&history, 1).await;

        assert_exact_switch(&store, patch, &[&cold_user, &cold_reply]).await;
    })
    .await
    .expect("switch_provenance_pages_past_the_first_window_and_resolves_exact exceeded its deadline");
}

#[tokio::test]
async fn switch_provenance_refuses_a_session_past_the_pool_bound() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        const SMALL_POOL_MAX_ROWS: usize = 1_000;
        let store = ProvenanceStore::new("t5e-pool-bound");
        store.unrelated_rows("bounded", 600).await;
        let messages = [ChatMessage::user("bounded unrelated question 0")];
        let durable = messages.iter().collect::<Vec<_>>();
        let forms = SourceForms::new(&durable);
        let principal = MemoryPrincipal {
            legacy_session_key: None,
            ..store.session.memory_principal()
        };

        let bounded = load_pool_within(store.memory.as_ref(), &principal, &forms, SMALL_POOL_MAX_ROWS)
            .await
            .expect("test: the pool reads");
        assert_eq!(
            bounded.err(),
            Some(ProvenanceGap::TurnAmbiguous),
            "a session past the pool bound has an unproven pool"
        );

        let complete = load_complete_pool(store.memory.as_ref(), &principal, &forms)
            .await
            .expect("test: the pool reads")
            .expect("test: the default bound reads the whole session");
        assert_eq!(
            complete.message_ids.len(),
            1_200,
            "the default bound pages to the session's first row"
        );
        assert_eq!(complete.messages.len(), 1, "only the matching row is kept");
    })
    .await
    .expect("switch_provenance_refuses_a_session_past_the_pool_bound exceeded its deadline");
}

#[tokio::test]
async fn cold_window_over_500_events_folds_the_oldest_500() {
    tokio::time::timeout(PROVENANCE_TEST_DEADLINE, async {
        const PAIRS: usize = 260;
        const MAX_EVENTS: usize = crate::tools::transcript_history_lookup::MAX_REFERENCED_EVENTS;
        let store = ProvenanceStore::new("m2-cold-window-cap");
        let mut history = vec![ChatMessage::system("sys")];
        let mut rows = Vec::new();
        for pair in 0..PAIRS {
            let run = format!("cap-run-{pair}");
            let question = format!("cold question {pair}");
            let answer = format!("cold answer {pair}");
            rows.push(store.user(Some(run.as_str()), &question).await);
            rows.push(store.assistant(Some(run.as_str()), &answer).await);
            history.push(ChatMessage::user(question));
            history.push(ChatMessage::assistant(answer));
        }
        store.user(Some("cap-tail"), "tail question").await;
        history.push(ChatMessage::user("tail question"));
        assert!(
            rows.len() > MAX_EVENTS,
            "premise: the requested cold window holds more durable messages than a handoff references"
        );

        let patch = store
            .build(&history, 1)
            .await
            .expect("test: an oversized cold window still switches");

        let note = switch_note(&patch);
        let oldest: Vec<&MessageEvent> = rows.iter().take(MAX_EVENTS).collect();
        assert_eq!(
            note_event_ids(&note),
            ids_of(&oldest),
            "the handoff references the oldest 500 rows in history order"
        );
        assert_eq!(
            (patch.range_start(), patch.range_end()),
            (1, 1 + MAX_EVENTS),
            "the switch folds exactly the oldest 500 messages"
        );
        let mut compacted = history.clone();
        apply_compaction_patch_exact(&mut compacted, &patch).expect("test: the patch applies to its source");
        let expected_tail: Vec<ChatMessage> = history.iter().skip(1 + MAX_EVENTS).cloned().collect();
        assert_eq!(
            compacted.get(2..).map(contents),
            Some(contents(&expected_tail)),
            "the rest of the cold window and the tail stay in the window for a later switch"
        );
        assert_eq!(
            store.switch_rows().await.len(),
            1,
            "exactly one switch row is persisted"
        );
    })
    .await
    .expect("cold_window_over_500_events_folds_the_oldest_500 exceeded its deadline");
}
