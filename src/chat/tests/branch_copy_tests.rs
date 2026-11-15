//! R3d: a branch copies its handoff chain and retained turns in one
//! transaction, and lookups from the branch read only the copies.
#![cfg(test)]

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tempfile::TempDir;

use crate::agent::compaction::{
    apply_compaction_patch_exact, build_configurable_compaction_patch, parse_context_handoff_note,
    resolve_compaction_event_provenance,
};
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::chat::branch_copy::copy_branch_transcript;
use crate::chat::chat_events::chat_runtime_envelope;
use crate::chat::session;
use crate::chat::session_store::branched_chat_session_from;
use crate::memory::branch_copy::{BranchCopyBatch, BranchCopyError, BranchCopyPlan, BranchCopySessionKeys};
use crate::memory::{Memory, MemoryFabric, NoneMemory, SqliteMemory};
use crate::providers::{ChatMessage, Provider};
use crate::runtime::envelope::RuntimeEnvelope;
use crate::tools::traits::Tool;
use crate::tools::transcript_history_lookup::TranscriptHistoryLookupTool;

const WORKSPACE: &str = "workspace-branch-copy";
/// Upper bound for one test, its real SQLite work and its cleanup.
const TEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
const FACT: &str = "the vault code is ORCHID-4471";

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

fn switch_config() -> crate::config::AgentCompactionConfig {
    crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Switch,
        reserve_tokens: 1,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 50,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    }
}

struct Chain {
    _temp: TempDir,
    memory: Arc<dyn Memory>,
    session: session::ChatSession,
    source: RuntimeEnvelope,
}

/// Append `pairs` unique user/assistant pairs to the history and the store.
async fn record_pairs(
    fabric: &MemoryFabric,
    envelope: &RuntimeEnvelope,
    history: &mut Vec<ChatMessage>,
    round: usize,
    pairs: usize,
) {
    for pair in 0..pairs {
        let user = if round == 0 && pair == 0 {
            format!("remember this: {FACT}. {}", "padding ".repeat(20))
        } else {
            format!("round {round} question {pair} {}", "padding ".repeat(20))
        };
        fabric
            .record_inbound_user_message(
                envelope.message_scope(),
                user.clone(),
                Some(format!("branch-copy-test-{round}-{pair}")),
                None,
            )
            .await
            .expect("test: record user");
        history.push(ChatMessage::user(user));
        let assistant = format!("round {round} answer {pair} {}", "detail ".repeat(20));
        fabric
            .record_assistant_message(envelope.message_scope(), assistant.clone())
            .await
            .expect("test: record assistant");
        history.push(ChatMessage::assistant(assistant));
    }
}

async fn compact(history: &mut Vec<ChatMessage>, audit: &DocumentIngestRuntime, trigger: &str) {
    let patch = build_configurable_compaction_patch(
        history,
        &UnusedProvider,
        "model",
        &switch_config(),
        Some(audit),
        trigger,
    )
    .await
    .expect("test: build patch")
    .expect("test: switch patch");
    apply_compaction_patch_exact(history, &patch).expect("test: apply patch");
}

/// A generation-2 chain whose copy exceeds 32 rows, plus retained turns.
async fn nested_chain() -> Chain {
    let temp = TempDir::new().expect("test: tempdir");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: sqlite"));
    let fabric = MemoryFabric::new(Arc::clone(&memory), WORKSPACE);
    let mut session = session::ChatSession::new("test-provider", "test-model");
    let source = chat_runtime_envelope(WORKSPACE, &format!("chat:{}", session.id));
    let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &source);
    let mut history = vec![ChatMessage::system("sys")];
    record_pairs(&fabric, &source, &mut history, 0, 20).await;
    compact(&mut history, &audit, "first").await;
    record_pairs(&fabric, &source, &mut history, 1, 12).await;
    compact(&mut history, &audit, "second").await;
    record_pairs(&fabric, &source, &mut history, 2, 1).await;
    session.turns = history
        .iter()
        .skip(1)
        .map(|message| session::ChatTurn {
            role: message.role.clone(),
            content: message.content.clone(),
            timestamp: chrono::Utc::now(),
            tool_calls: Vec::new(),
            context_handoff: false,
        })
        .collect();
    Chain {
        _temp: temp,
        memory,
        session,
        source,
    }
}

fn note_of(content: &str) -> serde_json::Value {
    parse_context_handoff_note(content).expect("test: handoff note")
}

async fn lookup(
    memory: &Arc<dyn Memory>,
    scope: &RuntimeEnvelope,
    note: &serde_json::Value,
) -> crate::tools::ToolResult {
    let arguments = note.pointer("/lookup/arguments").expect("test: lookup arguments");
    TranscriptHistoryLookupTool::new(Arc::clone(memory))
        .execute(json!({
            "event_ids": arguments.get("event_ids"),
            "first_row_id": arguments.get("first_row_id"),
            "last_row_id": arguments.get("last_row_id"),
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "workspace_id": scope.workspace_id,
                "session_key": scope.session_key,
                "channel": scope.channel,
                "sender": scope.sender,
                "owner_id": scope.resolved_owner_id()
            }
        }))
        .await
        .expect("test: lookup runs")
}

fn looked_up_contents(result: &crate::tools::ToolResult) -> Vec<String> {
    assert!(result.success, "lookup failed: {:?}", result.error);
    let output: serde_json::Value = serde_json::from_str(&result.output).expect("test: lookup json");
    output
        .get("events")
        .and_then(serde_json::Value::as_array)
        .expect("test: events")
        .iter()
        .filter_map(|event| event.get("content").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn a_branch_of_a_nested_chain_reads_the_fact_through_its_own_copies_only() {
    tokio::time::timeout(TEST_DEADLINE, async {
        let chain = nested_chain().await;
        let original_newest = note_of(&chain.session.turns.first().expect("test: handoff turn").content);
        assert_eq!(
            original_newest.get("to_generation"),
            Some(&json!(2)),
            "the fixture builds a generation-2 chain"
        );
        let first_batch_len = original_newest
            .pointer("/lookup/arguments/event_ids")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        assert!(first_batch_len > 0, "the newest handoff references rows");

        let keep = chain.session.turns.len();
        let mut branch = branched_chat_session_from(&chain.session, keep, "test-provider", "test-model");
        copy_branch_transcript(chain.memory.as_ref(), WORKSPACE, &chain.session.id, &mut branch)
            .await
            .expect("branch copy commits");
        let target = chat_runtime_envelope(WORKSPACE, &format!("chat:{}", branch.id));

        // The branch's handoff turn now points at copies: new identity, same generation.
        let newest = note_of(&branch.turns.first().expect("test: branch handoff").content);
        assert_eq!(
            newest.get("to_generation"),
            Some(&json!(2)),
            "the copied handoff keeps its generation"
        );
        assert_ne!(
            newest.get("handoff_event_id"),
            original_newest.get("handoff_event_id"),
            "the copied handoff has its own identity"
        );
        let parent = newest
            .get("parent_handoff_event_id")
            .and_then(serde_json::Value::as_str)
            .expect("copied parent is set");
        assert_ne!(
            Some(parent),
            original_newest
                .get("parent_handoff_event_id")
                .and_then(serde_json::Value::as_str),
            "the copied handoff's parent is the parent's copy, not the original"
        );

        // Generation 2 lookup from the branch returns the copied generation-1 handoff.
        let second = looked_up_contents(&lookup(&chain.memory, &target, &newest).await);
        let copied_first = second
            .iter()
            .find(|content| parse_context_handoff_note(content).is_some())
            .expect("generation-1 handoff copy is referenced");
        let first_note = note_of(copied_first);
        assert_eq!(
            first_note.get("handoff_event_id").and_then(serde_json::Value::as_str),
            Some(parent),
            "the generation-2 copy points at the generation-1 copy"
        );
        assert_eq!(
            first_note.get("parent_handoff_event_id"),
            Some(&serde_json::Value::Null),
            "the generation-1 copy is the root of the copied chain"
        );

        // Generation 1 lookup from the branch returns the fact, equal to the original row.
        let first = looked_up_contents(&lookup(&chain.memory, &target, &first_note).await);
        assert!(
            first.len() > 32,
            "the copy of generation 1 exceeds the 32-event batch limit"
        );
        let fact_copy = first
            .iter()
            .find(|content| content.contains(FACT))
            .expect("the fact is recovered from the branch");
        let original_first_note = {
            let original_second = looked_up_contents(&lookup(&chain.memory, &chain.source, &original_newest).await);
            note_of(
                original_second
                    .iter()
                    .find(|content| parse_context_handoff_note(content).is_some())
                    .expect("original generation-1 handoff"),
            )
        };
        let original_first = looked_up_contents(&lookup(&chain.memory, &chain.source, &original_first_note).await);
        assert_eq!(
            original_first.iter().find(|content| content.contains(FACT)),
            Some(fact_copy),
            "the branch recovers the fact byte-equal to the original row"
        );

        // The original session's ids are refused from the branch.
        let refused = lookup(&chain.memory, &target, &original_first_note).await;
        assert!(
            !refused.success,
            "a lookup of the original ids from the branch is refused"
        );
        assert!(
            refused
                .error
                .as_deref()
                .is_some_and(|error| error.contains("outside the authenticated session")),
            "the refusal names the session boundary: {:?}",
            refused.error
        );

        // Later compaction in the branch has exact provenance over its turns.
        let branch_messages = branch
            .turns
            .iter()
            .map(|turn| ChatMessage {
                role: turn.role.clone(),
                content: turn.content.clone(),
            })
            .collect::<Vec<_>>();
        let provenance =
            resolve_compaction_event_provenance(chain.memory.as_ref(), target.memory_principal(), &branch_messages)
                .await
                .expect("test: provenance read");
        assert_eq!(
            provenance.map(|provenance| provenance.source_event_ids.len()),
            Some(branch_messages.len()),
            "every branch turn has an exact copied row"
        );
    })
    .await
    .expect("a_branch_of_a_nested_chain_reads_the_fact_through_its_own_copies_only exceeded TEST_DEADLINE");
}

fn plan_with(ids: Vec<String>, source_session: &str) -> BranchCopyPlan {
    BranchCopyPlan {
        branch_id: "branch-test".to_string(),
        workspace_id: WORKSPACE.to_string(),
        source: BranchCopySessionKeys {
            session_key: RuntimeEnvelope::chat_canonical_session_key(source_session),
            legacy_session_key: format!("chat:{source_session}"),
        },
        target: BranchCopySessionKeys {
            session_key: RuntimeEnvelope::chat_canonical_session_key("branch-test"),
            legacy_session_key: "chat:branch-test".to_string(),
        },
        handoff_batches: Vec::new(),
        final_batch: BranchCopyBatch {
            original_event_ids: ids,
        },
    }
}

async fn branch_row_count(memory: &Arc<dyn Memory>) -> usize {
    // Chat rows are workspace-visible, so the listing also returns source rows;
    // count only the rows stored under the branch's session keys.
    let target = chat_runtime_envelope(WORKSPACE, "chat:branch-test");
    memory
        .list_message_events_since(&target.memory_principal(), 0, 5000)
        .await
        .expect("test: list branch rows")
        .iter()
        .filter(|event| {
            event
                .session_key
                .as_deref()
                .is_some_and(|key| key == target.session_key || key == "chat:branch-test")
        })
        .count()
}

#[tokio::test]
async fn a_failing_row_rolls_back_every_earlier_copy() {
    tokio::time::timeout(TEST_DEADLINE, async {
        let chain = nested_chain().await;
        let mut ids = resolve_compaction_event_provenance(
            chain.memory.as_ref(),
            chain.source.memory_principal(),
            &chain
                .session
                .turns
                .iter()
                .skip(1)
                .map(|turn| ChatMessage {
                    role: turn.role.clone(),
                    content: turn.content.clone(),
                })
                .collect::<Vec<_>>(),
        )
        .await
        .expect("test: provenance read")
        .expect("test: retained rows")
        .source_event_ids;
        ids.push("missing-event".to_string());
        let error = chain
            .memory
            .copy_events_for_branch(plan_with(ids, &chain.session.id))
            .await
            .expect_err("a missing row fails the copy");
        assert_eq!(
            error.downcast_ref::<BranchCopyError>(),
            Some(&BranchCopyError::SourceRowMissing {
                event_id: "missing-event".to_string()
            }),
            "the missing row is named in the typed error"
        );
        assert_eq!(
            branch_row_count(&chain.memory).await,
            0,
            "the failed copy rolled back every earlier row"
        );
    })
    .await
    .expect("a_failing_row_rolls_back_every_earlier_copy exceeded TEST_DEADLINE");
}

#[tokio::test]
async fn a_copy_over_the_row_limit_is_refused_before_any_write() {
    tokio::time::timeout(TEST_DEADLINE, async {
        let temp = TempDir::new().expect("test: tempdir");
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: sqlite"));
        let ids = (0..=crate::memory::branch_copy::BRANCH_COPY_MAX_ROWS)
            .map(|index| format!("event-{index}"))
            .collect::<Vec<_>>();
        let error = memory
            .copy_events_for_branch(plan_with(ids, "source-session"))
            .await
            .expect_err("too many rows");
        assert_eq!(
            error.downcast_ref::<BranchCopyError>(),
            Some(&BranchCopyError::TooLarge {
                rows: crate::memory::branch_copy::BRANCH_COPY_MAX_ROWS + 1,
                limit: crate::memory::branch_copy::BRANCH_COPY_MAX_ROWS,
            }),
            "an over-limit plan is refused with its size and the limit"
        );
        assert_eq!(branch_row_count(&memory).await, 0, "an over-limit plan writes nothing");
    })
    .await
    .expect("a_copy_over_the_row_limit_is_refused_before_any_write exceeded TEST_DEADLINE");
}

#[tokio::test]
async fn a_backend_without_an_event_log_refuses_the_copy_with_a_typed_error() {
    tokio::time::timeout(TEST_DEADLINE, async {
        let memory = NoneMemory::new();
        let error = memory
            .copy_events_for_branch(plan_with(vec!["event-1".to_string()], "source-session"))
            .await
            .expect_err("unsupported backend");
        assert_eq!(
            error.downcast_ref::<BranchCopyError>(),
            Some(&BranchCopyError::Unsupported),
            "a backend without an event log refuses the copy as unsupported"
        );
    })
    .await
    .expect("a_backend_without_an_event_log_refuses_the_copy_with_a_typed_error exceeded TEST_DEADLINE");
}

/// Referenced events of the largest handoff the restart test exercises.
const LARGE_HANDOFF_EVENTS: usize = 300;

/// Resume `session_id` from a freshly opened store in `workspace` (a real
/// restart: a new SQLite pool over the same file).
async fn restart_and_resume(workspace: &std::path::Path, session_id: &str) -> (Arc<dyn Memory>, session::ChatSession) {
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(workspace).expect("test: reopen sqlite"));
    let fabric = MemoryFabric::new(Arc::clone(&memory), WORKSPACE);
    let resumed = crate::chat::run::resume_session(
        Some(session_id),
        true,
        &memory,
        &fabric,
        &crate::config::Config::default(),
        "test-provider",
        "test-model",
    )
    .await
    .expect("test: resume after the restart");
    (memory, resumed)
}

/// The handoff turn of `session` and its validated note's referenced ids.
fn validated_handoff(session: &session::ChatSession, label: &str) -> (serde_json::Value, usize) {
    let turn = session
        .turns
        .iter()
        .find(|turn| crate::agent::compaction::is_context_handoff_turn(&turn.role, &turn.content))
        .unwrap_or_else(|| panic!("{label}: the session keeps its handoff turn"));
    assert!(
        turn.context_handoff,
        "{label}: the handoff turn keeps its compaction provenance"
    );
    let note = crate::agent::compaction::validated_context_handoff_note(&turn.content)
        .unwrap_or_else(|| panic!("{label}: the stored handoff note validates whole"));
    let referenced = note
        .pointer("/lookup/arguments/event_ids")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    (note, referenced)
}

#[tokio::test]
async fn the_largest_supported_handoff_survives_a_restart_and_a_branch_copy() {
    tokio::time::timeout(TEST_DEADLINE, async {
        let temp = TempDir::new().expect("test: tempdir");
        let session_id = {
            let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(temp.path()).expect("test: sqlite"));
            let fabric = MemoryFabric::new(Arc::clone(&memory), WORKSPACE);
            let mut state = crate::chat::acked_session::TestSessionDispatcher::state_with_history(Vec::new());
            let source = chat_runtime_envelope(WORKSPACE, &format!("chat:{}", state.session.id));
            let audit = DocumentIngestRuntime::from_envelope(Arc::clone(&memory), &source);
            let mut history = vec![ChatMessage::system("sys")];
            // keep_recent_messages = 2: the handoff references every row
            // but the last exchange.
            record_pairs(&fabric, &source, &mut history, 0, LARGE_HANDOFF_EVENTS / 2 + 1).await;
            let patch = build_configurable_compaction_patch(
                &history,
                &UnusedProvider,
                "model",
                &switch_config(),
                Some(&audit),
                "large",
            )
            .await
            .expect("test: build patch")
            .expect("test: the largest supported range yields a switch patch");
            // The reducer adopts it exactly as a chat does; its window is
            // large enough that nothing is trimmed afterwards.
            state.session.history = history;
            let _ = state.reduce(crate::chat::action::Action::HistoryCompactionPatchApplied {
                reason: crate::chat::action::CompactReason::Manual,
                patch,
                compaction_config: crate::config::AgentCompactionConfig {
                    max_context_tokens: 1_000_000,
                    ..switch_config()
                },
            });
            let compacted = state.build_session_snapshot();
            let (_, referenced) = validated_handoff(&compacted, "compacted");
            assert_eq!(
                referenced, LARGE_HANDOFF_EVENTS,
                "the handoff references the supported upper range"
            );
            crate::chat::session_store::save_session(memory.as_ref(), &compacted)
                .await
                .expect("test: save the compacted session");
            compacted.id
        };

        let (memory, resumed) = restart_and_resume(temp.path(), &session_id).await;
        let (note, referenced) = validated_handoff(&resumed, "restart");
        assert_eq!(
            referenced, LARGE_HANDOFF_EVENTS,
            "the resumed note keeps every referenced id"
        );
        let original = chat_runtime_envelope(WORKSPACE, &format!("chat:{session_id}"));
        assert!(
            looked_up_contents(&lookup(&memory, &original, &note).await)
                .iter()
                .any(|content| content.contains(FACT)),
            "the resumed note's lookup reaches the oldest fact"
        );

        let mut branch = branched_chat_session_from(&resumed, resumed.turns.len(), "test-provider", "test-model");
        copy_branch_transcript(memory.as_ref(), WORKSPACE, &session_id, &mut branch)
            .await
            .expect("test: the branch copy of the largest handoff commits");
        crate::chat::session_store::save_session(memory.as_ref(), &branch)
            .await
            .expect("test: save the branch");
        drop(memory);

        let (memory, resumed_branch) = restart_and_resume(temp.path(), &branch.id).await;
        let (branch_note, branch_referenced) = validated_handoff(&resumed_branch, "branch after restart");
        assert_eq!(
            branch_referenced, LARGE_HANDOFF_EVENTS,
            "the branch's copied note keeps every referenced id"
        );
        assert_ne!(
            branch_note.get("handoff_event_id"),
            note.get("handoff_event_id"),
            "the branch note references its own copy"
        );
        let target = chat_runtime_envelope(WORKSPACE, &format!("chat:{}", branch.id));
        assert!(
            looked_up_contents(&lookup(&memory, &target, &branch_note).await)
                .iter()
                .any(|content| content.contains(FACT)),
            "the branch's lookup reaches the oldest fact through its copies"
        );
    })
    .await
    .expect("the_largest_supported_handoff_survives_a_restart_and_a_branch_copy exceeded TEST_DEADLINE");
}
