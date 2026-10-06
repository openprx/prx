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
use crate::agent::document_ingest::DocumentIngestRuntime;
use crate::agent::history::build_native_assistant_history;
use crate::agent::turn_contracts::OS_PAGING_RECALL_MARKER;
use crate::agent::turn_tests::{first_orphan_tool_index, paging_test_config};
use crate::memory::{
    DocumentIngestInput, DocumentRecord, DocumentSearchResult, Memory, MemoryCategory, MemoryEntry, MemoryPrincipal,
    MessageEvent, MessageEventInput, SqliteMemory,
};
use crate::providers::{ChatMessage, ToolCall};
use crate::runtime::envelope::RuntimeEnvelope;
use async_trait::async_trait;
use std::sync::Arc;
use tempfile::TempDir;

struct FailingIngestMemory {
    inner: Arc<dyn Memory>,
}

#[async_trait]
impl Memory for FailingIngestMemory {
    fn name(&self) -> &str {
        "failing-ingest-memory"
    }
    fn supports_document_ingest(&self) -> bool {
        true
    }
    async fn ingest_document(&self, input: DocumentIngestInput) -> anyhow::Result<DocumentRecord> {
        let _ = input;
        anyhow::bail!("forced ingest failure")
    }
    async fn search_document_chunks(
        &self,
        principal: &MemoryPrincipal,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<DocumentSearchResult>> {
        self.inner.search_document_chunks(principal, query, limit).await
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
        self.inner.append_message_event(input).await
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

/// Recalled pages are verbatim transcripts of evicted turns. Their markers
/// belong to those turns and must not be resolved as attachments of the new
/// one — the recall role is configurable, so `system` is not a safeguard.
#[tokio::test]
async fn recalled_context_pages_carry_no_live_image_markers() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();
    let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent(workspace, "run-paging-markers");
    let runtime = DocumentIngestRuntime::from_envelope(mem, &envelope);

    let mut config = paging_test_config(true);
    config.os_paging.retrieval_injection_role = crate::config::RetrievalInjectionRole::User;

    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user(format!(
            "ZEBRA-PANGOLIN receipt [IMAGE:/gone/artifacts/receipt.jpg] {}",
            "please read it. ".repeat(40)
        )),
        ChatMessage::assistant("Noted the receipt. ".repeat(40)),
        ChatMessage::user("latest question please summarize"),
    ];

    assert!(apply_os_paging(&mut history, &config, Some(&runtime)).await.unwrap());

    let recalled = recall_context_pages("ZEBRA-PANGOLIN receipt", &config, Some(&runtime))
        .await
        .expect("evicted page should be recalled");
    assert_eq!(recalled.role, "user");
    assert!(
        recalled.content.contains("receipt.jpg"),
        "the reference must stay readable: {}",
        recalled.content
    );
    assert!(
        !recalled.content.contains("[IMAGE:"),
        "a recalled page must not re-arm an old image marker: {}",
        recalled.content
    );
    assert_eq!(
        crate::multimodal::count_image_markers(std::slice::from_ref(&recalled)),
        0,
        "a user-role recall must contribute no resolvable image references"
    );
}

#[test]
fn switch_mode_bypasses_legacy_os_paging() {
    let mut config = paging_test_config(true);
    assert!(context_mode_uses_os_paging(&config));

    config.mode = crate::config::AgentCompactionMode::Switch;
    assert!(
        !context_mode_uses_os_paging(&config),
        "an exact handoff must run before any legacy paging can evict its source messages"
    );
}

#[tokio::test]
async fn os_paging_disabled_is_a_noop_zero_regression() {
    // With paging disabled, apply_os_paging must never touch history even
    // when it is well above the eviction threshold.
    let config = paging_test_config(false);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("u1 ".repeat(120)),
        ChatMessage::assistant("a1 ".repeat(120)),
        ChatMessage::user("u2 ".repeat(120)),
        ChatMessage::assistant("a2 ".repeat(120)),
    ];
    let before = history.clone();
    let evicted = apply_os_paging(&mut history, &config, None).await.unwrap();
    assert!(!evicted, "disabled paging must not evict");
    assert_eq!(history.len(), before.len());
    assert!(
        history.iter().all(|m| !m.content.starts_with(OS_PAGING_EVICT_MARKER)),
        "no paging breadcrumb should be injected when disabled"
    );
}

#[tokio::test]
async fn os_paging_below_threshold_does_not_evict() {
    let config = paging_test_config(true);
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("short message")];
    let before = history.clone();
    let evicted = apply_os_paging(&mut history, &config, None).await.unwrap();
    assert!(!evicted);
    assert_eq!(history.len(), before.len());
}

#[tokio::test]
async fn os_paging_evicts_cold_context_into_document_store_and_recalls() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();
    let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent(workspace, "run-paging-1");
    let runtime = DocumentIngestRuntime::from_envelope(mem, &envelope);

    let config = paging_test_config(true);
    let mut history = vec![
        ChatMessage::system("sys"),
        // Distinctive token to assert verbatim recall later.
        ChatMessage::user("Remember the secret deployment token ZEBRA-PANGOLIN-42. ".repeat(40)),
        ChatMessage::assistant("Acknowledged the deployment token. ".repeat(40)),
        ChatMessage::user("And the database is at db-host-9001. ".repeat(40)),
        ChatMessage::assistant("Noted database host. ".repeat(40)),
        ChatMessage::user("latest question please summarize"),
    ];
    let original_len = history.len();

    let evicted = apply_os_paging(&mut history, &config, Some(&runtime)).await.unwrap();
    assert!(evicted, "paging should evict cold context above threshold");
    // History should be shorter and contain a paging breadcrumb.
    assert!(history.len() < original_len);
    assert!(
        history.iter().any(|m| m.content.starts_with(OS_PAGING_EVICT_MARKER)),
        "a paging breadcrumb should replace evicted messages"
    );
    // The recent user message must remain in the hot window.
    assert!(history.iter().any(|m| m.content == "latest question please summarize"));

    // Recall the evicted page by querying the distinctive token.
    let recalled = recall_context_pages("ZEBRA-PANGOLIN deployment token", &config, Some(&runtime))
        .await
        .expect("evicted page should be recalled");
    assert_eq!(recalled.role, "system");
    assert!(recalled.content.starts_with(OS_PAGING_RECALL_MARKER));
    assert!(
        recalled.content.contains("ZEBRA-PANGOLIN-42"),
        "recalled page must preserve full content verbatim, got: {}",
        &recalled.content[..recalled.content.len().min(200)]
    );
}

/// A recalled page is a verbatim transcript of earlier turns, so it can carry
/// the very `[Used tools: ...]` lines that were written before 0.8.137
/// stopped them. Handing one back to the model is the same fabricated claim
/// arriving by a slower route.
///
/// MUTATION GUARD: drop the `strip_quoted_fabricated_tool_markers` call in
/// `recall_context_pages` and this goes red.
#[tokio::test]
async fn os_paging_recall_strips_fabricated_tool_markers_from_the_page_body() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();
    let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent(workspace, "run-paging-marker-strip");
    let runtime = DocumentIngestRuntime::from_envelope(mem, &envelope);

    let config = paging_test_config(true);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("ZEBRA-PANGOLIN disk check please. ".repeat(60)),
        ChatMessage::assistant(format!(
            "{}\n[Used tools: shell, session_status]",
            "ZEBRA-PANGOLIN root is at 41 percent. ".repeat(60)
        )),
        ChatMessage::user("latest question please summarize"),
    ];
    assert!(apply_os_paging(&mut history, &config, Some(&runtime)).await.unwrap());

    let recalled = recall_context_pages("ZEBRA-PANGOLIN disk check", &config, Some(&runtime))
        .await
        .expect("evicted page should be recalled");

    assert!(
        !recalled.content.contains("[Used tools:"),
        "a cold page must not carry a fabricated tool-use claim back into the prompt: {}",
        recalled.content
    );
    assert!(
        recalled.content.contains("ZEBRA-PANGOLIN root is at 41 percent."),
        "only the marker is dropped; the page itself is still recalled: {}",
        recalled.content
    );
}

/// A recalled page is a verbatim transcript of turns that already ran. The
/// injected message must say so in words the model cannot miss, and must
/// stamp every excerpt with its own source line, otherwise stale version
/// numbers and command output inside the page get replayed to the user as
/// this turn's freshly measured results (observed after a daemon restart:
/// a recalled page's old version numbers were reported as "just measured").
///
/// MUTATION GUARD: empty `OS_PAGING_RECALL_HISTORY_NOTICE`, or drop the
/// `page_source_line` write in `recall_context_pages`, and this goes red.
#[tokio::test]
async fn os_paging_recall_frames_pages_as_history_with_source_lines() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();
    let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent(workspace, "run-paging-history-notice");
    let runtime = DocumentIngestRuntime::from_envelope(mem, &envelope);

    let config = paging_test_config(true);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("ZEBRA-PANGOLIN release check: version 0.8.108 at HEAD fb3b30f4. ".repeat(40)),
        ChatMessage::assistant("Recorded the ZEBRA-PANGOLIN release numbers. ".repeat(40)),
        ChatMessage::user("latest question please summarize"),
    ];
    assert!(apply_os_paging(&mut history, &config, Some(&runtime)).await.unwrap());

    let recalled = recall_context_pages("ZEBRA-PANGOLIN release check", &config, Some(&runtime))
        .await
        .expect("evicted page should be recalled");

    // The legacy marker stays the first thing in the message: callers
    // (`latest_user_query`, injection filters) identify recalls by prefix.
    assert!(
        recalled.content.starts_with(OS_PAGING_RECALL_MARKER),
        "the recall marker must remain the message prefix: {}",
        &recalled.content[..recalled.content.len().min(160)]
    );

    // Literal phrases, not the constant: asserting against the constant
    // would still pass if the notice were emptied out.
    for phrase in [
        "verbatim transcript of EARLIER turns in this session",
        "describes the state at the time it was recorded, not the state now",
        "Do not present anything from them as something you did, ran, or verified in this turn",
        "call the appropriate tool again and report that fresh result instead",
    ] {
        assert!(
            recalled.content.contains(phrase),
            "recall message must carry the history declaration phrase {phrase:?}, got: {}",
            recalled.content
        );
    }

    // The declaration has to sit ahead of the excerpts, not after them.
    let notice_at = recalled
        .content
        .find("HISTORY NOTICE")
        .expect("history declaration must be present");
    let first_page_at = recalled
        .content
        .find(OS_PAGING_RECALL_PAGE_PREFIX)
        .expect("every excerpt must carry a [page ...] source line");
    assert!(
        notice_at < first_page_at,
        "the history declaration must precede the first excerpt: {}",
        recalled.content
    );

    // Each excerpt is introduced by its own source line carrying the
    // eviction time, and the transcript still follows it verbatim.
    assert!(
        recalled.content.contains("\n[page evicted_at="),
        "excerpts must be stamped with their eviction time: {}",
        recalled.content
    );
    let page_line = recalled
        .content
        .lines()
        .find(|line| line.starts_with(OS_PAGING_RECALL_PAGE_PREFIX))
        .expect("a [page ...] line must exist");
    assert!(
        page_line.ends_with(']') && page_line.len() > "[page evicted_at=]".len(),
        "the source line must carry a non-empty timestamp, got: {page_line}"
    );
    assert!(
        recalled.content.contains("ZEBRA-PANGOLIN release check"),
        "framing must not cost the verbatim page content: {}",
        recalled.content
    );
}

/// Search results carry chunk rows only, so a backend that cannot supply a
/// creation time must still get a source line — just without the field.
#[test]
fn page_source_line_drops_a_missing_timestamp_but_keeps_the_marker() {
    assert_eq!(
        page_source_line("2026-09-17T04:05:06Z"),
        "[page evicted_at=2026-09-17T04:05:06Z]"
    );
    assert_eq!(page_source_line("   "), "[page]");
    assert_eq!(page_source_line(""), "[page]");
}

/// OS-paging evicts a span from the middle of history and replaces it with
/// one breadcrumb. When that span ends between an assistant `tool_calls`
/// envelope and the result answering it, the result is left at the head of
/// the hot window with nothing to answer — the orphan shape every native
/// provider rejects. The round-trip must be paged out whole instead, so the
/// output also stays recallable.
///
/// MUTATION GUARD: delete the forward walk over stranded `role="tool"`
/// messages in `apply_os_paging` and the orphan assertion goes red.
#[tokio::test]
async fn os_paging_never_strands_a_tool_result_at_the_hot_window_head() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();
    let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let envelope = RuntimeEnvelope::agent(workspace, "run-paging-orphan");
    let runtime = DocumentIngestRuntime::from_envelope(mem, &envelope);

    let mut config = paging_test_config(true);
    // Lands the requested eviction boundary exactly on the second tool
    // result, i.e. inside a round-trip.
    config.os_paging.keep_recent_messages = 2;
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("first question ".repeat(40)),
        ChatMessage::assistant(build_native_assistant_history(
            "",
            &[ToolCall {
                id: "call-1".to_string(),
                name: "shell".to_string(),
                arguments: "{}".to_string(),
            }],
            None,
        )),
        ChatMessage::tool(
            serde_json::json!({"tool_call_id": "call-1", "content": "first result ".repeat(40)}).to_string(),
        ),
        ChatMessage::assistant("first answer ".repeat(40)),
        ChatMessage::user("second question ".repeat(40)),
        ChatMessage::assistant(build_native_assistant_history(
            "",
            &[ToolCall {
                id: "call-2".to_string(),
                name: "shell".to_string(),
                arguments: "{}".to_string(),
            }],
            None,
        )),
        ChatMessage::tool(
            serde_json::json!({"tool_call_id": "call-2", "content": "PANGOLIN-TOOL-OUTPUT ".repeat(40)}).to_string(),
        ),
        ChatMessage::user("latest question please summarize"),
    ];
    assert_eq!(history.len(), 9);
    assert_eq!(
        history.get(7).map(|message| message.role.as_str()),
        Some("tool"),
        "fixture is useless unless the requested boundary cuts inside a round-trip"
    );

    let evicted = apply_os_paging(&mut history, &config, Some(&runtime)).await.unwrap();

    assert!(evicted, "paging should evict cold context above threshold");
    assert_eq!(
        first_orphan_tool_index(&history),
        None,
        "paging must not strand a tool result: {history:?}"
    );
    assert_eq!(
        history.last().map(|message| message.content.as_str()),
        Some("latest question please summarize"),
        "the newest turn must stay in the hot window"
    );
    assert!(
        history.iter().any(|m| m.content.starts_with(OS_PAGING_EVICT_MARKER)),
        "a paging breadcrumb should replace the evicted span"
    );

    // The stranded round-trip was paged out, not dropped: it is recallable.
    let recalled = recall_context_pages("PANGOLIN tool output", &config, Some(&runtime))
        .await
        .expect("the evicted round-trip must be recallable");
    assert!(
        recalled.content.contains("PANGOLIN-TOOL-OUTPUT"),
        "the tool output must have reached the page store, not been discarded"
    );
}

#[tokio::test]
async fn os_paging_ingest_failure_leaves_history_unchanged() {
    let tmp = TempDir::new().unwrap();
    let workspace = tmp.path().to_string_lossy().to_string();
    let inner: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let mem: Arc<dyn Memory> = Arc::new(FailingIngestMemory { inner });
    let envelope = RuntimeEnvelope::agent(workspace, "run-paging-fail");
    let runtime = DocumentIngestRuntime::from_envelope(mem, &envelope);

    let config = paging_test_config(true);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("u1 ".repeat(120)),
        ChatMessage::assistant("a1 ".repeat(120)),
        ChatMessage::user("u2 ".repeat(120)),
        ChatMessage::assistant("a2 ".repeat(120)),
        ChatMessage::user("latest question"),
    ];
    let before_roles: Vec<String> = history.iter().map(|msg| msg.role.clone()).collect();
    let before_contents: Vec<String> = history.iter().map(|msg| msg.content.clone()).collect();

    let evicted = apply_os_paging(&mut history, &config, Some(&runtime)).await.unwrap();
    assert!(!evicted, "ingest failure must soft-fail without eviction");
    assert_eq!(
        history.iter().map(|msg| msg.role.clone()).collect::<Vec<_>>(),
        before_roles
    );
    assert_eq!(
        history.iter().map(|msg| msg.content.clone()).collect::<Vec<_>>(),
        before_contents
    );
}

#[test]
fn inject_recalled_context_inserts_after_system() {
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("hello")];
    inject_recalled_context(&mut history, ChatMessage::system("recalled"));
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].content, "sys");
    assert_eq!(history[1].content, "recalled");
    assert_eq!(history[2].content, "hello");
}
