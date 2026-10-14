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
use crate::agent::turn_tests::{
    CONTEXT_BUDGET_FIXTURE_RESERVE_TOKENS, DelayTool, NativeScriptedProvider, RecordingTool, RefreshCountingTool,
    ScriptedProvider, SystemSummaryProvider, WIDE_SCHEMA_TOOL_BODY_MARKER, WideSchemaTool,
    context_budget_fixture_config, context_budget_fixture_history, first_orphan_tool_index, tool_specs_of,
};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::memory::{
    DocumentIngestInput, DocumentRecord, Memory, MemoryCategory, MemoryEntry, MessageEvent, MessageEventInput,
    SqliteMemory,
};
use crate::observability::NoopObserver;
use crate::providers::ChatResponse;
use crate::providers::traits::ProviderCapabilities;
use crate::tools::ToolCategory;
use tempfile::TempDir;

struct NonVisionProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for NonVisionProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok("ok".to_string())
    }
}

struct VisionProvider {
    calls: Arc<AtomicUsize>,
    expected_user_text: Option<&'static str>,
}

#[async_trait]
impl Provider for VisionProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: false,
            vision: true,
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok("ok".to_string())
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let marker_count = crate::multimodal::count_image_markers(request.messages);
        if marker_count == 0 {
            anyhow::bail!("expected image markers in request messages");
        }

        if let Some(expected_text) = self.expected_user_text {
            let user_text_found = request
                .messages
                .iter()
                .any(|message| message.role == "user" && message.content.contains(expected_text));
            if !user_text_found {
                anyhow::bail!("expected user text '{expected_text}' in multimodal request messages");
            }
        }

        if request.tools.is_some() {
            anyhow::bail!("no tools should be attached for this test");
        }

        Ok(ChatResponse {
            text: Some("vision-ok".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

/// Records the exact payload handed to the provider so a test can assert on
/// what the model really received, not on what the caller intended to send.
struct PayloadCapturingProvider {
    captured: Arc<Mutex<Vec<ChatMessage>>>,
}

#[async_trait]
impl Provider for PayloadCapturingProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: false,
            vision: true,
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("ok".to_string())
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        if let Ok(mut captured) = self.captured.lock() {
            *captured = request.messages.to_vec();
        }
        Ok(ChatResponse {
            text: Some("answered".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

/// Lower bound on the serialized size of one fixture tool, in bytes.
const WIDE_SCHEMA_TOOL_DESCRIPTION_CHARS: usize = 4_000;

/// A tool with a one-line description and an empty schema: the control for
/// the oversized fixture above.
struct NarrowSchemaTool {
    name: &'static str,
}

#[async_trait]
impl Tool for NarrowSchemaTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: "ok".to_string(),
            error: None,
        })
    }
}

/// Declares native tool calling so the loop publishes real native tool
/// specs instead of prompt-guided instructions.
struct NativeToolProvider {
    text: &'static str,
    calls: Arc<AtomicUsize>,
    max_request_tokens: Arc<AtomicUsize>,
}

impl NativeToolProvider {
    fn returning(text: &'static str) -> Self {
        Self {
            text,
            calls: Arc::new(AtomicUsize::new(0)),
            max_request_tokens: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Spy form: reports how often it was reached and the largest complete
    /// request it was asked to serve.
    fn recording(text: &'static str, calls: &Arc<AtomicUsize>, max_request_tokens: &Arc<AtomicUsize>) -> Self {
        Self {
            text,
            calls: Arc::clone(calls),
            max_request_tokens: Arc::clone(max_request_tokens),
        }
    }
}

#[async_trait]
impl Provider for NativeToolProvider {
    fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
        crate::providers::traits::ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("summary".to_string())
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let tool_payload = crate::agent::turn_context::measure_native_tool_payload(request.tools.unwrap_or_default());
        let request_tokens = estimate_request_tokens(
            estimate_history_tokens_with_source(request.messages),
            tool_payload.measurement,
        );
        self.max_request_tokens.fetch_max(request_tokens, Ordering::SeqCst);
        Ok(ChatResponse {
            text: Some(self.text.to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

struct RecordingBudgetProvider {
    first_call_tokens: Arc<AtomicUsize>,
}

struct CapturingCompactionProvider {
    captured_history: Arc<Mutex<Option<Vec<ChatMessage>>>>,
}

#[async_trait]
impl Provider for RecordingBudgetProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("summary".to_string())
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        self.first_call_tokens
            .compare_exchange(
                0,
                estimate_history_tokens(request.messages),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .ok();
        Ok(ChatResponse {
            text: Some("done".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

#[async_trait]
impl Provider for CapturingCompactionProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok("## Decisions\n- preserve compacted context\n## Open TODOs\n- answer the latest user question\n## Critical Context\n- ISS-037 capture test".to_string())
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        let last_user = request
            .messages
            .iter()
            .rfind(|message| message.role == "user")
            .map(|message| message.content.clone())
            .unwrap_or_default();
        *self
            .captured_history
            .lock()
            .expect("captured history lock should be valid") = Some(request.messages.to_vec());
        Ok(ChatResponse {
            text: Some(format!("answer bound to: {last_user}")),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

/// Stands in for a chat template that accepts exactly one system message,
/// at index 0 (the deployed Qwen template is one). It also holds the
/// corrective retry to the prefix contract: the system message it sees on
/// the retry must be the byte-identical one it saw first.
struct StrictSystemPositionRetryProvider {
    calls: AtomicUsize,
    first_system: Mutex<Option<String>>,
}

#[async_trait]
impl Provider for StrictSystemPositionRetryProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        anyhow::bail!("chat_with_system should not be used in strict system-position tests");
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        if request
            .messages
            .iter()
            .enumerate()
            .any(|(index, message)| message.role == "system" && index != 0)
        {
            anyhow::bail!("system message must be at the beginning");
        }
        let leading_system = request
            .messages
            .first()
            .filter(|message| message.role == "system")
            .map(|message| message.content.clone())
            .unwrap_or_default();
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            if let Ok(mut first) = self.first_system.lock() {
                *first = Some(leading_system);
            }
            return Ok(ChatResponse {
                text: Some(String::new()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            });
        }
        let first_system = self
            .first_system
            .lock()
            .ok()
            .and_then(|first| first.clone())
            .unwrap_or_default();
        if leading_system != first_system {
            anyhow::bail!("the corrective retry moved the system prefix");
        }
        let tail_carries_instruction = request.messages.last().is_some_and(|message| {
            message.role == "user" && message.content.contains(EMPTY_ASSISTANT_RESPONSE_RETRY_MESSAGE)
        });
        if !tail_carries_instruction {
            anyhow::bail!("corrective retry instruction did not reach the tail of the request");
        }
        Ok(ChatResponse {
            text: Some("Recovered answer.".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

/// Answers the first call with an empty response so the loop takes its
/// corrective-retry path, then echoes the tail user turn back verbatim:
/// exactly what a model does when it reads the corrective instruction as
/// the operator's latest request.
struct EchoTailRetryProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for EchoTailRetryProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        anyhow::bail!("chat_with_system should not be used in the echo fixture");
    }

    async fn chat(&self, request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Ok(ChatResponse {
                text: Some(String::new()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            });
        }
        let echoed = request
            .messages
            .last()
            .map(|message| message.content.clone())
            .unwrap_or_default();
        Ok(ChatResponse {
            text: Some(echoed),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

/// The corrective instruction is the runtime talking, not the operator, so
/// it must be attributable wherever it surfaces. A model is free to quote
/// the turn it was handed back at the user, and nothing downstream strips
/// it; what the runtime owes is that every copy it puts into the request is
/// labelled, so an echo can be recognised and cut at the marker instead of
/// being read as the operator's own words.
///
/// MUTATION GUARD: drop `CORRECTIVE_ASIDE_PREFIX` from
/// `append_corrective_aside` and the split below finds no marker.
#[tokio::test]
async fn a_corrective_aside_stays_attributable_when_the_model_echoes_it() {
    use futures::FutureExt;
    let tmp = TempDir::new().expect("test: tempdir").keep();
    let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
        .await
        .expect("owned actual Answer decision endpoint")
        .retain_evidence_at(tmp.clone());
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(true, || async {
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .expect("actual corrective-aside cleanup owner");
            let caught = cleanup
                .run(async {
                    Ok(std::panic::AssertUnwindSafe(async {
                        let provider = EchoTailRetryProvider {
                            calls: AtomicUsize::new(0),
                        };
                        let mut history = vec![
                            ChatMessage::system("sys"),
                            ChatMessage::user("summarise the release notes"),
                        ];
                        let admission = crate::agent::turn_tests::admission::AdmittedTurn::configured_answer(
                            tmp.as_path(),
                            "summarise the release notes",
                            &endpoint,
                        )
                        .await;
                        let result = run_turn(
                            &mut history,
                            crate::agent::turn_request::TurnRequest {
                                channel_name: "cli",
                                scope: None,
                                mode: ChatMode::default(),
                                silent: true,
                                expose_stay_silent: false,
                                multimodal: &crate::config::MultimodalConfig::default(),
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: 2,
                                    priority_scheduling_enabled: false,
                                    low_priority_tool_names: Vec::new(),
                                },
                                compaction: None,
                                decision: admission.decision(),
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: &provider,
                                provider_name: "ollama",
                                model: "prx-local-model",
                                temperature: 0.0,
                                tools_registry: Arc::new(Vec::new()),
                                observer: &NoopObserver,
                                hooks: &HookManager::new(tmp.as_path().to_path_buf()),
                                approval: None,
                                approval_resolver: None,
                                runtime_adapter: Some(admission.adapter()),
                                memory: ToolLoopMemory::none(),
                            },
                            crate::agent::turn_request::TurnDelivery {
                                cancellation: None,
                                on_delta: None,
                                on_tool_call: None,
                            },
                        )
                        .await;
                        let _ = admission.decision().context.terminal_execution_trace();
                        let (outcome, _trace) = result.expect("test: the corrective retry must complete the turn");

                        let ToolLoopOutcome::Text(answer) = outcome else {
                            panic!("test: the echo fixture returns text");
                        };
                        // Fixture proof: the model really did echo the instruction back.
                        assert!(
                            answer.contains(EMPTY_ASSISTANT_RESPONSE_RETRY_MESSAGE),
                            "the fixture must echo the corrective instruction: {answer}"
                        );
                        // Every copy of the instruction the runtime put into the request carries
                        // the marker, so the echo splits cleanly into operator text and aside.
                        let (operator_text, aside) = answer
                            .split_once(CORRECTIVE_ASIDE_PREFIX)
                            .expect("the corrective instruction must be labelled wherever it is emitted");
                        assert_eq!(
                            operator_text.trim(),
                            "summarise the release notes",
                            "everything before the marker must be exactly what the operator wrote: {answer}"
                        );
                        assert!(
                            aside.trim().starts_with("The previous assistant response was empty."),
                            "the marker must sit immediately before the runtime's instruction: {aside}"
                        );
                        // And the aside was only ever a request-local edit: the operator's own
                        // turn in the conversation is untouched, so the next turn cannot carry
                        // the instruction forward.
                        assert_eq!(
                            history
                                .iter()
                                .find(|message| message.role == "user")
                                .map(|message| message.content.as_str()),
                            Some("summarise the release notes"),
                            "the corrective aside must not be written back into the operator's turn: {history:?}"
                        );
                    })
                    .catch_unwind()
                    .await)
                })
                .await
                .expect("actual service queues and owner must drain");
            if let Err(panic) = caught {
                std::panic::resume_unwind(panic);
            }
        })
        .await;
}

/// FIX-P2-02: a thin `Memory` decorator that counts `ingest_document` calls
/// by document-id prefix and delegates everything else to an inner backend.
/// Used to prove the native-mode tool loop persists each tool result (the
/// untrusted external copy *and* the oversized `tool_output` document) exactly
/// once — a duplicate write is otherwise invisible because ingest is an
/// idempotent UPSERT keyed on a deterministic, content-addressed id.
struct CountingIngestMemory {
    inner: Arc<dyn Memory>,
    untrusted_copy_ingests: Arc<AtomicUsize>,
    tool_output_ingests: Arc<AtomicUsize>,
}

#[async_trait]
impl Memory for CountingIngestMemory {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn supports_document_ingest(&self) -> bool {
        self.inner.supports_document_ingest()
    }
    async fn ingest_document(&self, input: DocumentIngestInput) -> anyhow::Result<DocumentRecord> {
        let id = input.document_id.clone().unwrap_or_default();
        if id.starts_with("untrusted:") {
            self.untrusted_copy_ingests.fetch_add(1, Ordering::SeqCst);
        } else if id.starts_with("tool-output:") {
            self.tool_output_ingests.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.ingest_document(input).await
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
    async fn find_owned_message_event(
        &self,
        workspace_id: &str,
        owner_id: &str,
        event_id: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.inner
            .find_owned_message_event(workspace_id, owner_id, event_id)
            .await
    }
    async fn find_owned_message_event_by_idempotency_key(
        &self,
        workspace_id: &str,
        owner_id: &str,
        idempotency_key: &str,
    ) -> anyhow::Result<Option<MessageEvent>> {
        self.inner
            .find_owned_message_event_by_idempotency_key(workspace_id, owner_id, idempotency_key)
            .await
    }
}

struct LargeOutputTool {
    output: String,
}

#[async_trait]
impl Tool for LargeOutputTool {
    fn name(&self) -> &str {
        "large_output"
    }

    fn description(&self) -> &str {
        "Returns a large output for document ingest tests"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: self.output.clone(),
            error: None,
        })
    }
}

/// FIX-P2-02: large-output tool with a configurable name so a test can use the
/// `web_fetch` name (an untrusted-external-content tool) while still producing
/// oversized output that also triggers the `tool_output` document path.
struct NamedLargeOutputTool {
    name: String,
    output: String,
}

#[async_trait]
impl Tool for NamedLargeOutputTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Returns a large output under a configurable tool name for dedup tests"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: self.output.clone(),
            error: None,
        })
    }
}

/// FIX-P2-02: in native mode the same tool result must be persisted exactly
/// once. Previously the generic loop *and* the native-history branch each
/// persisted both the untrusted external copy and the oversized tool_output
/// document, doubling I/O and storage. This drives a native-mode `web_fetch`
/// call with oversized output and asserts a single copy of each.
#[tokio::test]
async fn run_tool_call_loop_native_mode_persists_each_tool_output_once() {
    // Size the payload off the constant so it stays "large" regardless of
    // Behavior-limits Phase 1 raising MAX_TOOL_RESULT_INLINE_CHARS.
    let segment = "large native payload ";
    let repeats = MAX_TOOL_RESULT_INLINE_CHARS / segment.len() + 100;
    let large_output = format!("{}\n{}", "needle native dedup marker", segment.repeat(repeats));
    assert!(large_output.len() > MAX_TOOL_RESULT_INLINE_CHARS);

    let provider = NativeScriptedProvider::new(vec![
        ChatResponse {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "web_fetch".to_string(),
                arguments: "{}".to_string(),
            }],
            reasoning_content: None,
        },
        ChatResponse {
            text: Some("done".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        },
    ]);
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(NamedLargeOutputTool {
        name: "web_fetch".to_string(),
        output: large_output.clone(),
    })];
    let tmp = TempDir::new().unwrap();
    let inner: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let untrusted_copy_ingests = Arc::new(AtomicUsize::new(0));
    let tool_output_ingests = Arc::new(AtomicUsize::new(0));
    let memory: Arc<dyn Memory> = Arc::new(CountingIngestMemory {
        inner: Arc::clone(&inner),
        untrusted_copy_ingests: Arc::clone(&untrusted_copy_ingests),
        tool_output_ingests: Arc::clone(&tool_output_ingests),
    });
    let input = "Use web_fetch.";
    let mut history = vec![ChatMessage::system("test-system"), ChatMessage::user(input)];
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let mut config = crate::config::Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
        config,
        input,
        Arc::clone(&tools_registry),
        Arc::clone(&memory),
    )
    .await;
    admission.assert_core_specs(&tool_specs_of(&tools_registry));

    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &crate::hooks::HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: admission.memory_runtime_with_ingest(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect("native dedup tool loop should complete");
    assert_eq!(result, "done");

    // Persistence uses deterministic, content-addressed document ids, so a
    // duplicate write is an idempotent UPSERT — invisible at the document
    // level. The counting decorator records every `ingest_document` call, so
    // the previously-buggy native branch (which persisted both the untrusted
    // copy and the oversized tool_output document a second time) would show a
    // count of 2 here. Each must be exactly 1.
    assert_eq!(
        untrusted_copy_ingests.load(Ordering::SeqCst),
        1,
        "untrusted external copy must be persisted exactly once in native mode (no duplicate write)"
    );
    assert_eq!(
        tool_output_ingests.load(Ordering::SeqCst),
        1,
        "oversized tool_output document must be persisted exactly once in native mode (no duplicate write)"
    );

    // Sanity: the untrusted copy is in the store and correctly tagged, while
    // the recall hard-filter would still exclude it from trusted context.
    let mut chunks = inner
        .search_document_chunks(&admission.principal(), "needle native dedup marker", 50)
        .await
        .unwrap();
    assert!(
        chunks
            .iter()
            .any(|r| r.source_kind.as_deref() == Some("untrusted_external")),
        "untrusted external copy should be present in the store for provenance"
    );
    drop_untrusted_external_results(&mut chunks);
    assert!(
        chunks
            .iter()
            .all(|r| r.source_kind.as_deref() != Some("untrusted_external")),
        "recall hard-filter must drop the untrusted external copy"
    );
}

/// FIX #2 (#3): a native-tool provider that scripts BOTH the response and the
/// `ChatTrace` attribution per turn. The first turn returns a tool call while
/// reporting a serving provider that *deviates* from the routed selection (a
/// fallback). The second turn returns clean final text on the routed
/// provider. This lets `run_turn` exercise the real
/// cross-turn `any_turn_had_fallback` aggregation, not a `from_trace` helper.
struct TraceScriptedProvider {
    turns: Arc<Mutex<VecDeque<ScriptedTurn>>>,
}

struct ScriptedTurn {
    response: ChatResponse,
    final_provider: String,
    /// Number of attempts to record (>1 also signals a fallback).
    attempts: Vec<crate::llm::route_decision::ProviderAttempt>,
}

#[async_trait]
impl Provider for TraceScriptedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        anyhow::bail!("chat_with_system unused in trace-scripted provider tests")
    }

    async fn chat_traced(
        &self,
        _request: ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        let turn = self
            .turns
            .lock()
            .expect("turns lock valid")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("trace-scripted provider exhausted turns"))?;
        Ok(crate::providers::traits::ChatTrace {
            response: turn.response,
            attempts: turn.attempts,
            final_provider: turn.final_provider,
            final_model: model.to_string(),
            served_member: None,
            tokens_used: TokenUsage::default(),
        })
    }
}

fn success_attempt(seq: u32, provider: &str, model: &str) -> crate::llm::route_decision::ProviderAttempt {
    use crate::llm::route_decision::AttemptStatus;
    let now = chrono::Utc::now();
    crate::llm::route_decision::ProviderAttempt {
        seq,
        provider: provider.to_string(),
        model: model.to_string(),
        started_at: now,
        finished_at: now,
        status: AttemptStatus::Success,
        error_class: None,
        error_message: None,
    }
}

fn failed_attempt(seq: u32, provider: &str, model: &str) -> crate::llm::route_decision::ProviderAttempt {
    use crate::llm::route_decision::AttemptStatus;
    let now = chrono::Utc::now();
    crate::llm::route_decision::ProviderAttempt {
        seq,
        provider: provider.to_string(),
        model: model.to_string(),
        started_at: now,
        finished_at: now,
        status: AttemptStatus::Failed,
        error_class: Some("retryable".to_string()),
        error_message: Some("500 transient".to_string()),
    }
}

#[tokio::test]
async fn run_tool_call_loop_aggregates_intermediate_turn_fallback() {
    // #3: turn 1 (tool-call turn) falls back — its trace records a failed
    // attempt on the routed provider then a success on a DIFFERENT provider.
    // Turn 2 returns clean final text on the routed provider with a single
    // success attempt. The returned `ToolLoopTrace` must carry
    // `any_turn_had_fallback = true` so the chat layer records the whole user
    // turn as FallbackSuccess (not a clean Success) — locking the cross-turn
    // sticky-fallback aggregation end to end at the loop layer.
    let routed_provider = "ollama";
    let routed_model = "prx-local-model";

    let provider = TraceScriptedProvider {
        turns: Arc::new(Mutex::new(VecDeque::from(vec![
            // Turn 1: a native tool call, served by a DEVIATED provider after a
            // failed attempt on the routed one → this turn had a fallback.
            ScriptedTurn {
                response: ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call-1".to_string(),
                        name: "file_read".to_string(),
                        arguments: "{}".to_string(),
                    }],
                    reasoning_content: None,
                },
                final_provider: "deviated-provider".to_string(),
                attempts: vec![
                    failed_attempt(1, routed_provider, routed_model),
                    success_attempt(2, "deviated-provider", routed_model),
                ],
            },
            // Turn 2: clean final answer on the routed provider, single attempt.
            ScriptedTurn {
                response: ChatResponse {
                    text: Some("final clean answer".to_string()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                },
                final_provider: routed_provider.to_string(),
                attempts: vec![success_attempt(1, routed_provider, routed_model)],
            },
        ]))),
    };

    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
        name: "file_read".to_string(),
        execution_order: Arc::new(std::sync::Mutex::new(Vec::new())),
    })];
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("use file_read then answer"),
    ];
    let tmp = TempDir::new().unwrap();

    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
        tmp.path(),
        "use file_read then answer",
        Arc::clone(&tools_registry),
    )
    .await;
    let expected_specs = tools_registry
        .iter()
        .filter(|tool| tool.name() != "capability_search")
        .map(|tool| tool.spec())
        .collect::<Vec<_>>();
    admission.assert_selected_specs(&expected_specs);
    let (text, trace) = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: routed_provider,
            model: routed_model,
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &crate::hooks::HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, trace)| (outcome.into_text(), trace))
    .expect("multi-turn loop should complete");

    assert_eq!(text, "final clean answer");
    // The returning (final) turn was clean and on the routed provider...
    assert_eq!(trace.final_provider.as_deref(), Some(routed_provider));
    assert_eq!(trace.final_model.as_deref(), Some(routed_model));
    assert_eq!(trace.attempts.len(), 1, "final turn recorded a single success attempt");
    // ...but an EARLIER turn fell back, so the sticky aggregate flag is set.
    assert!(
        trace.any_turn_had_fallback,
        "an intermediate tool-call turn fell back → any_turn_had_fallback must be true"
    );

    // End-to-end: feed the loop trace into the same `from_trace` call the chat
    // layer uses. The clean final turn alone would be `Success`, but the
    // earlier-turn fallback flag must promote it to `FallbackSuccess` /
    // `earlier_turn_fallback`.
    use crate::llm::route_decision::{ExecutionStatus, RouteDecision};
    let decision = RouteDecision::single_candidate(routed_provider, routed_model);
    let now = chrono::Utc::now();
    let outcome = crate::llm::route_decision::ProviderExecutionOutcome::from_trace(
        &decision,
        trace.attempts,
        trace.final_provider.unwrap_or_default(),
        trace.final_model.unwrap_or_default(),
        now,
        now,
        trace.any_turn_had_fallback,
    );
    assert_eq!(
        outcome.status,
        ExecutionStatus::FallbackSuccess,
        "earlier-turn fallback must make the whole user turn a FallbackSuccess"
    );
    assert_eq!(outcome.fallback_reason.as_deref(), Some("earlier_turn_fallback"));
}

/// USERINFO-2: the loop names its provider in the request-context event
/// payload, as the `<provider>/<model>` recipient of that event, and in
/// every hook payload. A `custom:` URL may carry `user:password@`; none of
/// these durable or outbound records may carry it.
#[tokio::test]
async fn a_loop_turn_records_its_provider_without_url_userinfo() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let mut history = vec![ChatMessage::system("system"), ChatMessage::user("hello")];

    let mut config = crate::config::Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    config.cost.prices.insert(
        "custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1/mock-model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_model(
        config,
        &history.last().expect("source input").content,
        Arc::new(Vec::new()),
        Arc::clone(&memory),
        ("custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1", "mock-model"),
    )
    .await;
    let runtime = admission.memory_runtime();
    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "terminal",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 1,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &ScriptedProvider::from_text_responses(vec!["done"]),
            provider_name: "custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1",
            model: "mock-model",
            temperature: 0.0,
            tools_registry: Arc::new(Vec::new()),
            observer: &NoopObserver,
            hooks: &HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: runtime,
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .unwrap();
    assert_eq!(result, "done");

    let principal = admission.principal();
    let events = memory.list_message_events_since(&principal, 0, 20).await.unwrap();
    let event = events
        .iter()
        .find(|event| event.event_type == "llm.request.context.compiled")
        .expect("compiled request context event");
    assert_eq!(
        event.recipient.as_deref(),
        Some("custom:http://[redacted]@127.0.0.1:1/v1/mock-model")
    );
    let stored = format!("{events:?}");
    assert!(
        !stored.contains("SECRET-fixture-pw"),
        "a loop event kept the password: {stored}"
    );
}

#[tokio::test]
async fn compiled_request_context_is_persisted_with_transcript_lineage_without_prompt_content() {
    // Q52: evidence prerequisite only; preserve sensitive prompt exclusion and source lineage.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let mut history = vec![
            ChatMessage::system("private system instructions"),
            ChatMessage::user("hello"),
        ];

        let tools_registry = Arc::new(Vec::new());
        let mut config = crate::config::Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
            config,
            &history.last().expect("source input").content,
            Arc::clone(&tools_registry),
            Arc::clone(&memory),
        )
        .await;
        admission.assert_selected_specs(&[]);
        let runtime = admission.memory_runtime();

        let result = run_turn(
            &mut history,
            crate::agent::turn_request::TurnRequest {
                channel_name: "cli",
                scope: None,
                mode: ChatMode::default(),
                silent: true,
                expose_stay_silent: false,
                multimodal: &crate::config::MultimodalConfig::default(),
                schedule: crate::agent::turn_request::TurnSchedule {
                    read_only_tool_concurrency_window: 1,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: None,
                decision: admission.decision(),
            },
            crate::agent::turn_request::TurnServices {
                provider: &ScriptedProvider::from_text_responses(vec!["done"]),
                provider_name: "ollama",
                model: "prx-local-model",
                temperature: 0.0,
                tools_registry,
                observer: &NoopObserver,
                hooks: &HookManager::new(tmp.path().to_path_buf()),
                approval: None,
                approval_resolver: None,
                runtime_adapter: Some(admission.adapter()),
                memory: runtime,
            },
            crate::agent::turn_request::TurnDelivery {
                cancellation: None,
                on_delta: None,
                on_tool_call: None,
            },
        )
        .await
        .map(|(outcome, _trace)| outcome.into_text())
        .unwrap();
        assert_eq!(result, "done");

        let principal = admission.principal();
        let events = memory.list_message_events_since(&principal, 0, 10).await.unwrap();
        let event = events
            .iter()
            .find(|event| event.event_type == "llm.request.context.compiled")
            .expect("compiled request context event");
        let source = events
            .iter()
            .find(|event| event.role == "user" && event.event_type == "message.created")
            .expect("durable original user event");
        assert!(source.id > 0);
        assert_eq!(event.workspace_id, source.workspace_id);
        assert_eq!(event.owner_id, source.owner_id);
        assert_eq!(event.session_key, source.session_key);
        assert_eq!(event.run_id, source.run_id);
        assert_eq!(event.causation_event_id.as_deref(), Some(source.event_id.as_str()));
        let payload: serde_json::Value =
            serde_json::from_str(event.raw_payload_json.as_deref().expect("request context payload")).unwrap();
        assert_eq!(payload["schema_version"], 1);
        assert_eq!(payload["iteration"], 1);
        assert_eq!(payload["tool_snapshot"]["count"], 0);
        assert_eq!(payload["system_prompt"]["sha256"].as_str().map(str::len), Some(64));
        assert!(
            !event
                .raw_payload_json
                .as_deref()
                .unwrap_or_default()
                .contains("private system instructions"),
            "durable provenance must not duplicate system prompt content"
        );
    })
    .await;
}

/// Native tool schemas are part of the request the provider receives, so the
/// durable request event has to account for their size next to the prompt's.
/// Sizes, counts and estimates only: the event must never carry a schema
/// body, a prompt body or a tool argument.
#[tokio::test]
async fn compiled_request_event_accounts_for_prompt_and_tool_payload_sizes() {
    // Q45: Compiled-request telemetry accounts for real prompt and schema sizes.
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let tmp = TempDir::new().unwrap();
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(WideSchemaTool::new("file_read"))];
        let mut history = vec![
            ChatMessage::system("compact system instructions"),
            ChatMessage::user("Use file_read."),
        ];

        let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
        let mut config = crate::config::Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
            config,
            &history.last().expect("source input").content,
            Arc::clone(&tools_registry),
            Arc::clone(&memory),
        )
        .await;
        let expected_specs = tool_specs_of(&tools_registry)
            .into_iter()
            .filter(|spec| spec.name != "capability_search")
            .collect::<Vec<_>>();
        admission.assert_core_specs(&expected_specs);
        let runtime = admission.memory_runtime();

        let result = run_turn(
            &mut history,
            crate::agent::turn_request::TurnRequest {
                channel_name: "cli",
                scope: None,
                mode: ChatMode::default(),
                silent: true,
                expose_stay_silent: false,
                multimodal: &crate::config::MultimodalConfig::default(),
                schedule: crate::agent::turn_request::TurnSchedule {
                    read_only_tool_concurrency_window: 1,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: None,
                decision: admission.decision(),
            },
            crate::agent::turn_request::TurnServices {
                provider: &NativeToolProvider::returning("done"),
                provider_name: "ollama",
                model: "prx-local-model",
                temperature: 0.0,
                tools_registry,
                observer: &NoopObserver,
                hooks: &HookManager::new(tmp.path().to_path_buf()),
                approval: None,
                approval_resolver: None,
                runtime_adapter: Some(admission.adapter()),
                memory: runtime,
            },
            crate::agent::turn_request::TurnDelivery {
                cancellation: None,
                on_delta: None,
                on_tool_call: None,
            },
        )
        .await
        .map(|(outcome, _trace)| outcome.into_text())
        .unwrap();
        assert_eq!(result, "done");

        let principal = admission.principal();
        let events = memory.list_message_events_since(&principal, 0, 10).await.unwrap();
        let event = events
            .iter()
            .find(|event| event.event_type == "llm.request.context.compiled")
            .expect("compiled request context event");
        let raw = event.raw_payload_json.as_deref().expect("request context payload");
        let payload: serde_json::Value = serde_json::from_str(raw).unwrap();

        // The exact business tool is the only discoverable family, so the
        // request contains its schema without a redundant search control.
        assert_eq!(payload["tool_snapshot"]["count"], 1);
        assert_eq!(payload["tool_snapshot"]["sha256"].as_str().map(str::len), Some(64));

        let estimate = &payload["request_estimate"];
        let messages_bytes = estimate["messages_bytes"].as_u64().expect("messages byte count");
        let messages_chars = estimate["messages_chars"].as_u64().expect("messages char count");
        let messages_tokens = estimate["messages_tokens"].as_u64().expect("messages token estimate");
        let tool_bytes = estimate["native_tool_bytes"].as_u64().expect("native tool byte count");
        let tool_tokens = estimate["native_tool_tokens"]
            .as_u64()
            .expect("native tool token estimate");
        let total_tokens = estimate["total_tokens"].as_u64().expect("total request token estimate");

        assert!(messages_bytes > 0, "the prompt must be measured");
        assert!(messages_chars > 0, "the prompt must be measured in characters too");
        assert!(
            messages_tokens > 0,
            "the prompt token estimate must be deterministic and non-zero"
        );
        assert!(
            tool_bytes > u64::try_from(WIDE_SCHEMA_TOOL_DESCRIPTION_CHARS).unwrap_or(u64::MAX),
            "the serialized native tool payload must cover the whole schema, got {tool_bytes} bytes"
        );
        assert!(tool_tokens > 0, "the native tool payload must carry a token estimate");
        assert_eq!(
            total_tokens,
            messages_tokens.saturating_add(tool_tokens),
            "the reported total must be the prompt estimate plus the tool payload estimate"
        );
        assert_eq!(
            estimate["token_source"].as_str(),
            Some(if cfg!(feature = "llm-router") {
                "cl100k_proxy"
            } else {
                "heuristic"
            }),
            "the event must name the estimator it used"
        );

        assert!(
            !raw.contains(WIDE_SCHEMA_TOOL_BODY_MARKER),
            "durable provenance must not duplicate tool schema bodies"
        );
        assert!(
            !raw.contains("compact system instructions"),
            "durable provenance must not duplicate system prompt content"
        );
    })
    .await;
}

/// Number of alias specs the MCP fixture publishes beside its root.
const MCP_ALIAS_FIXTURE_ALIASES: usize = 3;

/// An `mcp_call` root that publishes itself plus a fixed set of aliases,
/// the way a live MCP router does after discovery.
struct McpAliasFixtureTool;

#[async_trait]
impl Tool for McpAliasFixtureTool {
    fn name(&self) -> &str {
        "mcp_call"
    }

    fn description(&self) -> &str {
        "mcp router"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn specs(&self) -> Vec<crate::tools::ToolSpec> {
        let mut specs = vec![self.spec()];
        specs.extend((0..MCP_ALIAS_FIXTURE_ALIASES).map(|index| crate::tools::ToolSpec {
            name: format!("mcp__fixture__alias_{index}"),
            description: "alias".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }));
        specs
    }

    fn supports_name(&self, name: &str) -> bool {
        name == "mcp_call" || name.starts_with("mcp__fixture__")
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        Ok(crate::tools::ToolResult {
            success: true,
            output: "ok".to_string(),
            error: None,
        })
    }
}

/// The compiled-request event splits the published tool surface into the
/// registry roots, the routed roots and the specs each adapter family
/// published (SPLASH-F4 R2).
///
/// One root may publish many specs. After real discovery admission, the
/// two business roots and search control publish their definitions plus
/// the MCP aliases. The event must distinguish roots from alias schemas.
#[tokio::test]
async fn compiled_request_event_breaks_the_tool_surface_down_by_root_and_adapter() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let tools_registry: Vec<Box<dyn Tool>> = vec![
        Box::new(NarrowSchemaTool { name: "plain_probe" }),
        Box::new(McpAliasFixtureTool),
    ];
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("Use plain_probe.")];

    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let mut config = crate::config::Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    let discovered = std::iter::once("mcp_call".to_owned())
        .chain((0..MCP_ALIAS_FIXTURE_ALIASES).map(|index| format!("mcp__fixture__alias_{index}")))
        .collect::<Vec<_>>();
    let final_rounds = vec![ChatResponse {
        text: Some("done".to_owned()),
        tool_calls: Vec::new(),
        reasoning_content: None,
    }];
    let mut rounds = vec![
        ChatResponse {
            text: None,
            tool_calls: discovered
                .iter()
                .enumerate()
                .map(|(index, name)| ToolCall {
                    id: format!("search-{index}"),
                    name: "capability_search".to_owned(),
                    arguments: serde_json::json!({"query": name, "limit": 1}).to_string(),
                })
                .collect(),
            reasoning_content: None,
        },
        ChatResponse {
            text: None,
            tool_calls: discovered
                .iter()
                .enumerate()
                .map(|(index, name)| ToolCall {
                    id: format!("append-{index}"),
                    name: name.clone(),
                    arguments: "{}".to_owned(),
                })
                .collect(),
            reasoning_content: None,
        },
    ];
    rounds.extend(final_rounds);
    let provider = NativeScriptedProvider::new(rounds);
    // All targeted fixture tools use native adapters with no filesystem/memory category.
    // Driver validates an actual use_tool/native_tool candidate in the compiled request.
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("native_tool")
        .await
        .expect("owned mechanics decision listener");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(true, || async {
            use futures::FutureExt as _;
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .expect("mechanics cleanup owner");
            let mut original_panic = None;
            let drained = cleanup
                .run(q79_boxed_fixture_work(async {
                    let caught = std::panic::AssertUnwindSafe(q79_boxed_fixture_work(async {
                        let admission = crate::agent::turn_tests::admission::AdmittedTurn::configured_native_mechanics(
                            config,
                            &history.last().expect("source input").content,
                            Arc::clone(&tools_registry),
                            Arc::clone(&memory),
                            &endpoint,
                            &provider,
                        )
                        .await;
                        let runtime = admission.memory_runtime();

                        let result = run_turn(
                            &mut history,
                            crate::agent::turn_request::TurnRequest {
                                channel_name: "cli",
                                scope: None,
                                mode: ChatMode::default(),
                                silent: true,
                                expose_stay_silent: false,
                                multimodal: &crate::config::MultimodalConfig::default(),
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: 1,
                                    priority_scheduling_enabled: false,
                                    low_priority_tool_names: Vec::new(),
                                },
                                compaction: None,
                                decision: admission.decision(),
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: &provider,
                                provider_name: "ollama",
                                model: "prx-local-model",
                                temperature: 0.0,
                                tools_registry,
                                observer: &NoopObserver,
                                hooks: &HookManager::new(tmp.path().to_path_buf()),
                                approval: None,
                                approval_resolver: None,
                                runtime_adapter: Some(admission.adapter()),
                                memory: runtime,
                            },
                            crate::agent::turn_request::TurnDelivery {
                                cancellation: None,
                                on_delta: None,
                                on_tool_call: None,
                            },
                        )
                        .await
                        .map(|(outcome, _trace)| outcome.into_text())
                        .unwrap();
                        assert_eq!(result, "done");

                        let principal = admission.principal();
                        let events = memory.list_message_events_since(&principal, 0, 256).await.unwrap();
                        let event = events
                            .iter()
                            .rev()
                            .find(|event| event.event_type == "llm.request.context.compiled")
                            .expect("compiled request context event");
                        let payload: serde_json::Value =
                            serde_json::from_str(event.raw_payload_json.as_deref().expect("request context payload"))
                                .unwrap();

                        assert_eq!(payload["iteration"], 3, "measure the actual appended surface");
                        let alias_count = u64::try_from(MCP_ALIAS_FIXTURE_ALIASES).unwrap();
                        assert_eq!(payload["tool_snapshot"]["count"], 3 + alias_count);
                        let surface = &payload["tool_surface"];
                        assert_eq!(surface["registry_roots"], 3, "{surface}");
                        assert_eq!(surface["routed_roots"], 3, "{surface}");
                        assert_eq!(surface["published_specs"], 3 + alias_count, "{surface}");
                        assert_eq!(surface["by_adapter"]["native"], 3, "{surface}");
                        assert_eq!(surface["by_adapter"]["mcp_alias"], alias_count, "{surface}");
                        assert_eq!(surface["by_adapter"]["skill"], 0, "{surface}");
                        assert_eq!(surface["by_adapter"]["wasm"], 0, "{surface}");
                        let published = surface["published_specs"].as_u64().expect("published spec count");
                        let routed = surface["routed_roots"].as_u64().expect("routed root count");
                        assert_eq!(
                            published - routed,
                            surface["by_adapter"]["mcp_alias"].as_u64().expect("alias count"),
                            "with one alias-publishing root, the specs beyond the roots are exactly its aliases"
                        );
                    }))
                    .catch_unwind()
                    .await;
                    if let Err(panic) = caught {
                        original_panic = Some(panic);
                    }
                    Ok(())
                }))
                .await;
            if let Some(panic) = original_panic {
                std::panic::resume_unwind(panic);
            }
            drained.expect("mechanics services must drain");
        })
        .await;
}

/// The compiled tool snapshot is rebuilt on every iteration so dynamic
/// backends cannot go stale, but rendering and fingerprinting it is
/// memoized for the turn. After discovery establishes the appended set,
/// two consecutive stable iterations must share the render and system prefix
/// used by provider-side prompt caches.
#[tokio::test]
async fn compiled_tool_snapshot_is_rendered_once_per_turn_with_a_stable_system_prefix() {
    let tmp = TempDir::new().unwrap();
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let final_rounds = vec![
        ChatResponse {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "delay_progress".to_string(),
                arguments: "{}".to_string(),
            }],
            reasoning_content: None,
        },
        ChatResponse {
            text: Some("done".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        },
    ];
    // Four tools registered out of name order: past the point where a
    // registry that happens to be sorted would hide an ordering bug.
    let tools_registry: Vec<Box<dyn Tool>> = vec![
        Box::new(SteadyTool {
            delay: Duration::from_millis(0),
            executions: Arc::new(AtomicUsize::new(0)),
        }),
        Box::new(RefreshCountingTool {
            name: "mcp_probe",
            category: ToolCategory::System,
            refreshes: Arc::new(AtomicUsize::new(0)),
        }),
        Box::new(RefreshCountingTool {
            name: "automation_probe",
            category: ToolCategory::Automation,
            refreshes: Arc::new(AtomicUsize::new(0)),
        }),
        Box::new(RefreshCountingTool {
            name: "extra_probe",
            category: ToolCategory::System,
            refreshes: Arc::new(AtomicUsize::new(0)),
        }),
    ];
    let mut history = vec![
        ChatMessage::system("stable base system prompt"),
        ChatMessage::user("Use delay_progress."),
    ];

    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let mut config = crate::config::Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    let discovered = ["mcp_probe", "automation_probe", "extra_probe"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut rounds = vec![
        ChatResponse {
            text: None,
            tool_calls: discovered
                .iter()
                .enumerate()
                .map(|(index, name)| ToolCall {
                    id: format!("search-{index}"),
                    name: "capability_search".to_owned(),
                    arguments: serde_json::json!({"query": name, "limit": 1}).to_string(),
                })
                .collect(),
            reasoning_content: None,
        },
        ChatResponse {
            text: None,
            tool_calls: discovered
                .iter()
                .enumerate()
                .map(|(index, name)| ToolCall {
                    id: format!("append-{index}"),
                    name: name.clone(),
                    arguments: "{}".to_owned(),
                })
                .collect(),
            reasoning_content: None,
        },
    ];
    rounds.extend(final_rounds);
    let provider = NativeScriptedProvider::new(rounds);
    // All targeted fixture tools use native adapters with no filesystem/memory category.
    // Driver validates an actual use_tool/native_tool candidate in the compiled request.
    let driver = crate::agent::agent::native_decision_fixture::Driver::start("native_tool")
        .await
        .expect("owned mechanics decision listener");
    let endpoint = driver.endpoint().to_owned();
    driver
        .run_factory(true, || async {
            use futures::FutureExt as _;
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .expect("mechanics cleanup owner");
            let mut original_panic = None;
            let drained = cleanup
                .run(q79_boxed_fixture_work(async {
                    let caught = std::panic::AssertUnwindSafe(q79_boxed_fixture_work(async {
                        let admission = crate::agent::turn_tests::admission::AdmittedTurn::configured_mechanics(
                            config,
                            &history.last().expect("source input").content,
                            Arc::clone(&tools_registry),
                            Arc::clone(&memory),
                            &endpoint,
                            &provider,
                        )
                        .await;
                        let runtime = admission.memory_runtime();

                        let result = run_turn(
                            &mut history,
                            crate::agent::turn_request::TurnRequest {
                                channel_name: "cli",
                                scope: None,
                                mode: ChatMode::default(),
                                silent: true,
                                expose_stay_silent: false,
                                multimodal: &crate::config::MultimodalConfig::default(),
                                schedule: crate::agent::turn_request::TurnSchedule {
                                    read_only_tool_concurrency_window: 2,
                                    priority_scheduling_enabled: false,
                                    low_priority_tool_names: Vec::new(),
                                },
                                compaction: None,
                                decision: admission.decision(),
                            },
                            crate::agent::turn_request::TurnServices {
                                provider: &provider,
                                provider_name: "ollama",
                                model: "prx-local-model",
                                temperature: 0.0,
                                tools_registry,
                                observer: &NoopObserver,
                                hooks: &HookManager::new(tmp.path().to_path_buf()),
                                approval: None,
                                approval_resolver: None,
                                runtime_adapter: Some(admission.adapter()),
                                memory: runtime,
                            },
                            crate::agent::turn_request::TurnDelivery {
                                cancellation: None,
                                on_delta: None,
                                on_tool_call: None,
                            },
                        )
                        .await
                        .map(|(outcome, _trace)| outcome.into_text())
                        .expect("discovery followed by two stable iterations should complete");
                        assert_eq!(result, "done");

                        let principal = admission.principal();
                        let events = memory.list_message_events_since(&principal, 0, 256).await.unwrap();
                        let mut payloads = events
                            .iter()
                            .filter(|event| event.event_type == "llm.request.context.compiled")
                            .filter_map(|event| event.raw_payload_json.as_deref())
                            .filter_map(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                            .collect::<Vec<_>>();
                        payloads.sort_by_key(|payload| payload["iteration"].as_u64().unwrap_or_default());
                        assert_eq!(payloads.len(), 4, "search, append and two stable iterations");
                        let payloads = &payloads[2..];
                        assert_eq!(payloads[0]["iteration"], 3);
                        assert_eq!(payloads[1]["iteration"], 4);

                        assert_eq!(payloads[0]["tool_snapshot"]["count"], 5);
                        assert_eq!(
                            payloads[0]["tool_snapshot"]["sha256"], payloads[1]["tool_snapshot"]["sha256"],
                            "an unchanged tool set must fingerprint identically across iterations"
                        );
                        assert_eq!(
                            payloads[0]["system_prompt"]["sha256"], payloads[1]["system_prompt"]["sha256"],
                            "the system prefix must stay byte-identical across iterations of one turn"
                        );
                        assert_eq!(payloads[0]["tool_snapshot_render"]["hits"], 1);
                        assert_eq!(payloads[0]["tool_snapshot_render"]["misses"], 2);
                        assert_eq!(
                            payloads[1]["tool_snapshot_render"]["hits"], 2,
                            "iteration 4 must reuse iteration 3's rendered snapshot"
                        );
                        assert_eq!(
                            payloads[1]["tool_snapshot_render"]["misses"], 2,
                            "iteration 4 must not re-render or re-fingerprint the snapshot"
                        );

                        let names = payloads[0]["tool_snapshot"]["tools"]
                            .as_array()
                            .expect("test: fingerprinted tool list")
                            .iter()
                            .filter_map(|tool| tool["name"].as_str())
                            .collect::<Vec<_>>();
                        assert_eq!(
                            names,
                            vec![
                                "capability_search",
                                "delay_progress",
                                "automation_probe",
                                "extra_probe",
                                "mcp_probe"
                            ],
                            "the sorted core stays first; discovered entries are sorted within the appended segment",
                        );
                    }))
                    .catch_unwind()
                    .await;
                    if let Err(panic) = caught {
                        original_panic = Some(panic);
                    }
                    Ok(())
                }))
                .await;
            if let Some(panic) = original_panic {
                std::panic::resume_unwind(panic);
            }
            drained.expect("mechanics services must drain");
        })
        .await;
}

#[tokio::test]
async fn run_tool_call_loop_returns_structured_error_for_non_vision_provider() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = NonVisionProvider {
        calls: Arc::clone(&calls),
    };

    let mut history = vec![ChatMessage::user(
        "please inspect [IMAGE:data:image/png;base64,iVBORw0KGgo=]".to_string(),
    )];
    let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
    let observer = NoopObserver;

    let workspace = TempDir::new().expect("multimodal fixture workspace");
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(
        workspace.path(),
        &history.last().expect("source input").content,
    )
    .await;
    let err = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            // no document ingest
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry: Arc::new(tools_registry),
            observer: &observer,
            hooks: &crate::hooks::HookManager::new(workspace.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            // no tool tiering
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            // no scope context
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect_err("provider without vision support should fail");

    assert!(err.to_string().contains("provider_capability_error"));
    assert!(err.to_string().contains("capability=vision"));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// R-C2BU F-5: the vision refusal names the provider; a `custom:` URL may
/// carry `user:password@`, and the channel logs this error verbatim.
#[tokio::test]
async fn the_vision_refusal_names_the_provider_without_url_userinfo() {
    let tmp = TempDir::new().expect("provider redaction workspace");
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).expect("redaction memory"));
    let provider = NonVisionProvider {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let mut history = vec![ChatMessage::user(
        "please inspect [IMAGE:data:image/png;base64,iVBORw0KGgo=]".to_string(),
    )];
    let mut config = crate::config::Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    config.cost.prices.insert(
        "custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1/mock-model".to_owned(),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_model(
        config,
        &history.last().expect("source input").content,
        Arc::new(Vec::new()),
        Arc::clone(&memory),
        ("custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1", "mock-model"),
    )
    .await;
    let err = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: "custom:http://u:SECRET-fixture-pw@127.0.0.1:1/v1",
            model: "mock-model",
            temperature: 0.0,
            tools_registry: Arc::new(Vec::<Box<dyn Tool>>::new()),
            observer: &NoopObserver,
            hooks: &crate::hooks::HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
    .expect_err("provider without vision support should fail");
    let rendered = format!("{err:#} {err:?}");
    assert!(rendered.contains("capability=vision"), "{rendered}");
    assert!(!rendered.contains("SECRET-fixture-pw"), "{rendered}");
}

#[tokio::test]
async fn run_tool_call_loop_rejects_oversized_image_payload() {
    crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = VisionProvider {
            calls: Arc::clone(&calls),
            expected_user_text: None,
        };

        let oversized_payload = STANDARD.encode(vec![0_u8; (1024 * 1024) + 1]);
        let mut history = vec![ChatMessage::user(format!(
            "Analyze this [IMAGE:data:image/png;base64,{oversized_payload}]"
        ))];

        let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
        let observer = NoopObserver;
        let multimodal = crate::config::MultimodalConfig {
            max_images: 4,
            max_image_size_mb: 1,
            allow_remote_fetch: false,
        };

        let workspace = TempDir::new().expect("multimodal fixture workspace");
        let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(
            workspace.path(),
            &history.last().expect("source input").content,
        )
        .await;
        let err = run_turn(
            &mut history,
            crate::agent::turn_request::TurnRequest {
                channel_name: "cli",
                scope: None,
                // no document ingest
                mode: ChatMode::default(),
                silent: true,
                expose_stay_silent: false,
                multimodal: &multimodal,
                schedule: crate::agent::turn_request::TurnSchedule {
                    read_only_tool_concurrency_window: 2,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: None,
                decision: admission.decision(),
            },
            crate::agent::turn_request::TurnServices {
                provider: &provider,
                provider_name: "ollama",
                model: "prx-local-model",
                temperature: 0.0,
                tools_registry: Arc::new(tools_registry),
                observer: &observer,
                hooks: &crate::hooks::HookManager::new(workspace.path().to_path_buf()),
                approval: None,
                approval_resolver: None,
                runtime_adapter: Some(admission.adapter()),
                // no tool tiering
                memory: ToolLoopMemory::none(),
            },
            crate::agent::turn_request::TurnDelivery {
                cancellation: None,
                on_delta: None,
                // no scope context
                on_tool_call: None,
            },
        )
        .await
        .map(|(outcome, _trace)| outcome.into_text())
        .expect_err("oversized payload must fail");

        assert!(
            err.to_string().contains("multimodal image size limit exceeded"),
            "unexpected oversized image refusal: {err:#}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    })
    .await;
}

#[tokio::test]
async fn run_tool_call_loop_accepts_valid_multimodal_request_flow() {
    // Q45: Valid image payload reaches the original vision provider once.
    q79_boxed_fixture_work(
        crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(q79_boxed_fixture_work(async {
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = VisionProvider {
                calls: Arc::clone(&calls),
                expected_user_text: None,
            };

            let mut history = vec![ChatMessage::user(
                "Analyze this [IMAGE:data:image/png;base64,iVBORw0KGgo=]".to_string(),
            )];
            let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
            let observer = NoopObserver;

            let workspace = TempDir::new().expect("multimodal fixture workspace");
            let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(
                workspace.path(),
                &history.last().expect("source input").content,
            )
            .await;
            let result = run_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
                    channel_name: "cli",
                    scope: None,
                    // no document ingest
                    mode: ChatMode::default(),
                    silent: true,
                    expose_stay_silent: false,
                    multimodal: &crate::config::MultimodalConfig::default(),
                    schedule: crate::agent::turn_request::TurnSchedule {
                        read_only_tool_concurrency_window: 2,
                        priority_scheduling_enabled: false,
                        low_priority_tool_names: Vec::new(),
                    },
                    compaction: None,
                    decision: admission.decision(),
                },
                crate::agent::turn_request::TurnServices {
                    provider: &provider,
                    provider_name: "ollama",
                    model: "prx-local-model",
                    temperature: 0.0,
                    tools_registry: Arc::new(tools_registry),
                    observer: &observer,
                    hooks: &crate::hooks::HookManager::new(workspace.path().to_path_buf()),
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(admission.adapter()),
                    // no tool tiering
                    memory: ToolLoopMemory::none(),
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: None,
                    on_delta: None,
                    // no scope context
                    on_tool_call: None,
                },
            )
            .await
            .map(|(outcome, _trace)| outcome.into_text())
            .expect("valid multimodal payload should pass");

            assert_eq!(result, "vision-ok");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        })),
    )
    .await;
}

/// Production incident 2026-08-22: a WhatsApp session went permanently mute.
/// The assistant had once quoted `src/channels/wacli.rs` — comment text that
/// contains a literal `[IMAGE:...]` — and that turn is replayed on every
/// later turn. Resolving it aborted the provider call before it was made, so
/// the user got nothing back. The turn must complete; only the phantom image
/// is missing.
#[tokio::test]
async fn quoted_image_marker_fossil_still_completes_the_turn() {
    // Q45: Quoted historical image marker does not alter current payload completion.
    q79_boxed_fixture_work(
        crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(q79_boxed_fixture_work(async {
            let captured = Arc::new(Mutex::new(Vec::new()));
            let provider = PayloadCapturingProvider {
                captured: Arc::clone(&captured),
            };

            let live_question = "N: does wacli support sending files?";
            let mut history = vec![ChatMessage::user(format!(
                "[Current session context]\n\
                 - [assistant:prx] Media markers (`[IMAGE:...]` etc.) are not mapped to `wacli send file`.\n\n\
                 {live_question}"
            ))];
            let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
            let observer = NoopObserver;

            let workspace = TempDir::new().expect("multimodal fixture workspace");
            let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(
                workspace.path(),
                &history.last().expect("source input").content,
            )
            .await;
            let result = run_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
                    channel_name: "cli",
                    scope: None,
                    mode: ChatMode::default(),
                    silent: true,
                    expose_stay_silent: false,
                    multimodal: &crate::config::MultimodalConfig::default(),
                    schedule: crate::agent::turn_request::TurnSchedule {
                        read_only_tool_concurrency_window: 2,
                        priority_scheduling_enabled: false,
                        low_priority_tool_names: Vec::new(),
                    },
                    compaction: None,
                    decision: admission.decision(),
                },
                crate::agent::turn_request::TurnServices {
                    provider: &provider,
                    provider_name: "ollama",
                    model: "prx-local-model",
                    temperature: 0.0,
                    tools_registry: Arc::new(tools_registry),
                    observer: &observer,
                    hooks: &crate::hooks::HookManager::new(workspace.path().to_path_buf()),
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(admission.adapter()),
                    memory: ToolLoopMemory::none(),
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: None,
                    on_delta: None,
                    on_tool_call: None,
                },
            )
            .await
            .map(|(outcome, _trace)| outcome.into_text())
            .expect("a quoted marker fossil must not abort the turn");

            assert_eq!(result, "answered");
            let payload = captured.lock().expect("captured payload").clone();
            let user_text: String = payload
                .iter()
                .filter(|message| message.role == "user")
                .map(|message| message.content.clone())
                .collect();
            assert!(
                user_text.contains(live_question),
                "the live question must reach the model: {user_text}"
            );
        })),
    )
    .await;
}

#[tokio::test]
async fn run_tool_call_loop_forwards_audio_transcript_with_image_payload() {
    // Q45: Audio transcript and image reach the original vision provider together.
    q79_boxed_fixture_work(
        crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(q79_boxed_fixture_work(async {
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = VisionProvider {
                calls: Arc::clone(&calls),
                expected_user_text: Some("Audio transcript: the package label says fragile"),
            };

            let mut history = vec![ChatMessage::user(
                "Audio transcript: the package label says fragile\nAnalyze the attached image too \
                 [IMAGE:data:image/png;base64,iVBORw0KGgo=]"
                    .to_string(),
            )];
            let tools_registry: Vec<Box<dyn Tool>> = Vec::new();
            let observer = NoopObserver;

            let workspace = TempDir::new().expect("multimodal fixture workspace");
            let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(
                workspace.path(),
                &history.last().expect("source input").content,
            )
            .await;
            let result = run_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
                    channel_name: "cli",
                    scope: None,
                    // no document ingest
                    mode: ChatMode::default(),
                    silent: true,
                    expose_stay_silent: false,
                    multimodal: &crate::config::MultimodalConfig::default(),
                    schedule: crate::agent::turn_request::TurnSchedule {
                        read_only_tool_concurrency_window: 2,
                        priority_scheduling_enabled: false,
                        low_priority_tool_names: Vec::new(),
                    },
                    compaction: None,
                    decision: admission.decision(),
                },
                crate::agent::turn_request::TurnServices {
                    provider: &provider,
                    provider_name: "ollama",
                    model: "prx-local-model",
                    temperature: 0.0,
                    tools_registry: Arc::new(tools_registry),
                    observer: &observer,
                    hooks: &crate::hooks::HookManager::new(workspace.path().to_path_buf()),
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(admission.adapter()),
                    // no tool tiering
                    memory: ToolLoopMemory::none(),
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: None,
                    on_delta: None,
                    // no scope context
                    on_tool_call: None,
                },
            )
            .await
            .map(|(outcome, _trace)| outcome.into_text())
            .expect("image plus audio transcript should reach the vision provider");

            assert_eq!(result, "vision-ok");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        })),
    )
    .await;
}

/// Records entry into the backend and returns a failed tool result. Its
/// counter distinguishes backend failure from rejection before dispatch.
struct RefusingTool {
    name: &'static str,
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for RefusingTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Refuses every call"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(crate::tools::ToolResult {
            success: false,
            output: String::new(),
            error: Some("fixture backend refused the operation".to_string()),
        })
    }
}

/// A tool that takes a fixed slice of time and counts its invocations, so a
/// test can assert both how long a turn really ran and how many rounds of
/// real work it did.
struct SteadyTool {
    delay: Duration,
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for SteadyTool {
    fn name(&self) -> &str {
        "delay_progress"
    }

    fn description(&self) -> &str {
        "Sleeps for a fixed interval and reports progress"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } }
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        tokio::time::sleep(self.delay).await;
        let round = self.executions.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(crate::tools::ToolResult {
            success: true,
            output: format!("tick {round}"),
            error: None,
        })
    }
}

#[cfg(feature = "wasm-plugins")]
#[tokio::test]
async fn shared_tool_loop_applies_wasm_middleware_to_delivered_text_and_history() {
    // Q45: Wasm middleware transforms delivered text and persisted history consistently.
    q79_boxed_fixture_work(
        crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(q79_boxed_fixture_work(async {
            let temp = TempDir::new().unwrap();
            let plugin_dir = temp.path().join("plugins/pipeline-middleware");
            std::fs::create_dir_all(&plugin_dir).unwrap();
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/pipeline-middleware");
            std::fs::copy(fixture.join("plugin.toml"), plugin_dir.join("plugin.toml")).unwrap();
            std::fs::copy(fixture.join("plugin.wasm"), plugin_dir.join("plugin.wasm")).unwrap();

            let runtime = crate::plugins::init_plugin_runtime(temp.path(), None).await.unwrap();
            let hooks = crate::hooks::HookManager::new(temp.path().to_path_buf());
            hooks.set_plugin_runtime(runtime).await;
            let provider = ScriptedProvider::from_text_responses(vec!["plain response"]);
            let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("hello")];
            let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(temp.path(), "hello").await;
            let (outcome, _) = run_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
                    channel_name: "whatsapp",
                    scope: None,
                    mode: ChatMode::default(),
                    silent: true,
                    expose_stay_silent: false,
                    multimodal: &crate::config::MultimodalConfig::default(),
                    schedule: crate::agent::turn_request::TurnSchedule {
                        read_only_tool_concurrency_window: 2,
                        priority_scheduling_enabled: false,
                        low_priority_tool_names: Vec::new(),
                    },
                    compaction: None,
                    decision: admission.decision(),
                },
                crate::agent::turn_request::TurnServices {
                    provider: &provider,
                    provider_name: "ollama",
                    model: "prx-local-model",
                    temperature: 0.0,
                    tools_registry: Arc::new(Vec::new()),
                    observer: &NoopObserver,
                    hooks: &hooks,
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(admission.adapter()),
                    memory: ToolLoopMemory::none(),
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: None,
                    on_delta: None,
                    on_tool_call: None,
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                outcome,
                ToolLoopOutcome::Text(ref text) if text == "[wasm-middleware] plain response"
            ));
            assert_eq!(
                history.last().map(|message| message.content.as_str()),
                Some("[wasm-middleware] plain response")
            );
        })),
    )
    .await;
}

/// `Outbound` runs exactly once per accepted, delivered turn.
///
/// The defect these cover is that outbound was invoked once per loop
/// iteration instead of once per accepted delivered turn: a tool round, a
/// corrective retry and a silent decision each bought the plugin another
/// execution — another WASM instantiation, another epoch deadline, another
/// round of whatever side effects the plugin owns.
///
/// The committed middleware fixture is the transformation under test; the
/// invocation count comes from [`outbound_middleware_calls`], because the
/// middleware chain accepts compiled components only.
#[cfg(feature = "wasm-plugins")]
mod outbound_middleware_lifecycle {
    use super::*;

    /// Install the committed middleware fixture in a temporary workspace and
    /// bind its plugin runtime to a fresh `HookManager`.
    async fn hooks_with_middleware(workspace: &Path) -> crate::hooks::HookManager {
        let plugin_dir = workspace.join("plugins/pipeline-middleware");
        std::fs::create_dir_all(&plugin_dir).expect("test: create the fixture plugin directory");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk/rust/examples/pipeline-middleware");
        std::fs::copy(fixture.join("plugin.toml"), plugin_dir.join("plugin.toml"))
            .expect("test: copy the fixture manifest");
        std::fs::copy(fixture.join("plugin.wasm"), plugin_dir.join("plugin.wasm"))
            .expect("test: copy the fixture component");
        let runtime = crate::plugins::init_plugin_runtime(workspace, None)
            .await
            .expect("test: initialize the workspace plugin runtime");
        let hooks = crate::hooks::HookManager::new(workspace.to_path_buf());
        hooks.set_plugin_runtime(runtime).await;
        hooks
    }

    /// Occurrences of the fixture's marker, i.e. how many times this exact
    /// string was transformed by the outbound stage.
    fn marker_count(text: &str) -> usize {
        text.matches("[wasm-middleware] ").count()
    }

    /// A middleware plugin cannot put a fabricated tool-use marker into the
    /// delivered reply.
    ///
    /// The `[Used tools: ...]` guard runs above the stage, on the model's
    /// own text, so it never sees what a plugin writes. Without the strip
    /// below the stage, a plugin could put a marker into the returned text
    /// and into the in-memory history, where nothing else in the turn would
    /// take it out again.
    ///
    /// This covers the post-outbound step in isolation, not the end-to-end
    /// path: the middleware chain accepts compiled components only, and the
    /// committed fixture prefixes `[wasm-middleware] `, so no installable
    /// middleware here can produce a marker to carry through the loop.
    #[test]
    fn outbound_middleware_output_cannot_inject_a_fabricated_tool_marker() {
        assert_eq!(
            finalize_outbound_display_text("The report is ready.", "[Used tools: file_read]\nThe report is ready."),
            "The report is ready.",
            "a marker the plugin introduced must not reach the delivered text or the history"
        );
    }

    /// The strip below the stage costs nothing when there is nothing to
    /// strip: both the untouched transform and a transform that carries no
    /// marker come back borrowed.
    #[test]
    fn outbound_middleware_output_without_a_marker_is_not_reallocated() {
        assert!(
            matches!(
                finalize_outbound_display_text("The report is ready.", "The report is ready."),
                Cow::Borrowed(_)
            ),
            "middleware that changes nothing leaves text that already passed the guard above"
        );
        assert!(
            matches!(
                finalize_outbound_display_text("The report is ready.", "[wasm-middleware] The report is ready."),
                Cow::Borrowed("[wasm-middleware] The report is ready.")
            ),
            "a transform that carries no marker is delivered exactly as the plugin returned it"
        );
    }

    /// The mid-loop preamble prints pre-outbound text, and that is pinned
    /// here at source level: `print!` goes to the process stdout, which this
    /// harness gives no stable hook to capture inside a test.
    ///
    /// What this pins is the reachability, not the bytes: the one preamble
    /// statement sits below the accepted-terminal return, so it runs only on
    /// an iteration that carries tool calls — an iteration on which the
    /// outbound block, which lives above that return inside
    /// `if tool_calls.is_empty()`, never executed. What it does not pin is
    /// what the preamble actually writes to stdout.
    #[test]
    fn outbound_middleware_does_not_reach_the_mid_loop_preamble() {
        let source = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/agent/turn_engine.rs"))
            .expect("test: read the tool loop source");
        let preamble = "print!(\"{display_text}\")";
        assert_eq!(
            source.matches(preamble).count(),
            1,
            "the mid-loop preamble must stay a single statement for this guard to mean anything"
        );
        let outbound = source
            .find("MiddlewareStage::Outbound,")
            .expect("test: locate the outbound call site");
        let accepted_return = source
            .find("return Ok((ToolLoopOutcome::Text(display_text), last_turn_trace));")
            .expect("test: locate the accepted terminal return");
        let preamble_at = source.find(preamble).expect("test: locate the mid-loop preamble");
        assert!(
            outbound < accepted_return && accepted_return < preamble_at,
            "the preamble must stay below the accepted-terminal return, i.e. on the tool-call path \
                 that the outbound stage never runs on"
        );
    }

    #[tokio::test]
    async fn outbound_middleware_runs_once_after_a_tool_round() {
        reset_outbound_middleware_calls();
        let temp = TempDir::new().expect("test: temporary workspace");
        let hooks = hooks_with_middleware(temp.path()).await;
        let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let provider = NativeScriptedProvider::new(vec![
            ChatResponse {
                text: Some("Checking the report.".to_string()),
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "file_read".to_string(),
                    arguments: "{}".to_string(),
                }],
                reasoning_content: None,
            },
            ChatResponse {
                text: Some("The report is ready.".to_string()),
                tool_calls: Vec::new(),
                reasoning_content: None,
            },
        ]);
        let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
            name: "file_read".to_string(),
            execution_order: Arc::clone(&execution_order),
        })];
        let mut history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("Use file_read to read the report."),
        ];
        let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
        let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
            temp.path(),
            &history.last().expect("source input").content,
            Arc::clone(&tools_registry),
        )
        .await;
        let (outcome, _trace) = run_turn(
            &mut history,
            crate::agent::turn_request::TurnRequest {
                channel_name: "cli",
                scope: None,
                mode: ChatMode::default(),
                silent: true,
                expose_stay_silent: false,
                multimodal: &crate::config::MultimodalConfig::default(),
                schedule: crate::agent::turn_request::TurnSchedule {
                    read_only_tool_concurrency_window: 2,
                    priority_scheduling_enabled: false,
                    low_priority_tool_names: Vec::new(),
                },
                compaction: None,
                decision: admission.decision(),
            },
            crate::agent::turn_request::TurnServices {
                provider: &provider,
                provider_name: "ollama",
                model: "prx-local-model",
                temperature: 0.0,
                tools_registry,
                observer: &NoopObserver,
                hooks: &hooks,
                approval: None,
                approval_resolver: None,
                runtime_adapter: Some(admission.adapter()),
                memory: admission.memory_runtime(),
            },
            crate::agent::turn_request::TurnDelivery {
                cancellation: None,
                on_delta: None,
                on_tool_call: None,
            },
        )
        .await
        .expect("test: a turn with one tool round must complete");

        assert_eq!(
            execution_order.lock().expect("test: execution order lock").as_slice(),
            ["file_read".to_string()],
            "the premise is a turn that really executed one tool round"
        );
        assert_eq!(
            outbound_middleware_calls(),
            1,
            "outbound must run once per accepted delivered turn, not once per loop iteration"
        );

        let ToolLoopOutcome::Text(ref delivered) = outcome else {
            panic!("expected a delivered text answer, got {outcome:?}");
        };
        assert_eq!(delivered, "[wasm-middleware] The report is ready.");
        assert_eq!(
            marker_count(delivered),
            1,
            "the accepted terminal text is transformed exactly once: {delivered}"
        );

        let assistant_turns: Vec<&str> = history
            .iter()
            .filter(|message| message.role == "assistant")
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(
            assistant_turns.len(),
            2,
            "one tool-requesting turn and one delivered turn: {history:?}"
        );
        assert!(
            marker_count(assistant_turns[0]) == 0 && assistant_turns[0].contains("Checking the report."),
            "the tool-call iteration keeps its pre-outbound representation: {:?}",
            assistant_turns[0]
        );
        assert_eq!(
            assistant_turns[1], delivered,
            "the accepted terminal text is stored once and returned once"
        );
        assert_eq!(
            history
                .iter()
                .filter(|message| message.content.contains("[wasm-middleware] "))
                .count(),
            1,
            "only the delivered turn carries a transformation: {history:?}"
        );
    }

    #[tokio::test]
    async fn outbound_middleware_skips_corrective_retry_responses() {
        // Q52: evidence prerequisite only; preserve fabricated-marker retry must not reach outbound middleware.
        q79_boxed_fixture_work(crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(q79_boxed_fixture_work(async {
            reset_outbound_middleware_calls();
            let temp = TempDir::new().expect("test: temporary workspace");
            let hooks = hooks_with_middleware(temp.path()).await;
            // The first response is a tool-summary-only line, which the terminal
            // branch rejects and replaces with one corrective retry. It is never
            // delivered, so it must never reach the outbound stage.
            let provider =
                ScriptedProvider::from_text_responses(vec!["[Used tools: file_read]", "The report is ready."]);
            let remaining = Arc::clone(&provider.responses);
            let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("read the report")];
            let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(
                temp.path(),
                &history.last().expect("source input").content,
            )
            .await;
            let (outcome, _trace) = run_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
                    channel_name: "cli",
                    scope: None,
                    mode: ChatMode::default(),
                    silent: true,
                    expose_stay_silent: false,
                    multimodal: &crate::config::MultimodalConfig::default(),
                    schedule: crate::agent::turn_request::TurnSchedule {
                        read_only_tool_concurrency_window: 2,
                        priority_scheduling_enabled: false,
                        low_priority_tool_names: Vec::new(),
                    },
                    compaction: None,
                    decision: admission.decision(),
                },
                crate::agent::turn_request::TurnServices {
                    provider: &provider,
                    provider_name: "ollama",
                    model: "prx-local-model",
                    temperature: 0.0,
                    tools_registry: Arc::new(Vec::new()),
                    observer: &NoopObserver,
                    hooks: &hooks,
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(admission.adapter()),
                    memory: admission.memory_runtime(),
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: None,
                    on_delta: None,
                    on_tool_call: None,
                },
            )
            .await
            .expect("test: the corrective retry must produce a deliverable answer");

            assert_eq!(
                remaining.lock().expect("test: responses lock").len(),
                0,
                "the premise is a rejected response followed by an accepted one (two provider calls)"
            );
            assert_eq!(
                outbound_middleware_calls(),
                1,
                "a response a corrective retry replaces is never delivered, so outbound must not run on it"
            );
            assert!(
                matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "[wasm-middleware] The report is ready."),
                "only the accepted response is transformed: {outcome:?}"
            );
            assert_eq!(
                history.last().map(|message| message.content.as_str()),
                Some("[wasm-middleware] The report is ready.")
            );
        })))
        .await;
    }

    #[tokio::test]
    async fn outbound_middleware_skips_silent_non_delivery() {
        // Q45: Middleware delivery accounting omits intentional silent non-delivery.
        crate::runtime::turn_decision::evidence_fixture::with_evidence_sufficient(async {
            reset_outbound_middleware_calls();
            let temp = TempDir::new().expect("test: temporary workspace");
            let hooks = hooks_with_middleware(temp.path()).await;
            let provider = ScriptedProvider::from_text_responses(vec![
                "<tool_call>\n{\"name\":\"stay_silent\",\"arguments\":{\"reason\":\"off-topic banter\"}}\n</tool_call>",
            ]);
            let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::StaySilentTool::new())];
            let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("banter")];
            let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
            let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
                temp.path(),
                &history.last().expect("source input").content,
                Arc::clone(&tools_registry),
            )
            .await;
            let (outcome, _trace) = run_turn(
                &mut history,
                crate::agent::turn_request::TurnRequest {
                    channel_name: "cli",
                    scope: None,
                    mode: ChatMode::default(),
                    silent: true,
                    expose_stay_silent: true,
                    multimodal: &crate::config::MultimodalConfig::default(),
                    schedule: crate::agent::turn_request::TurnSchedule {
                        read_only_tool_concurrency_window: 2,
                        priority_scheduling_enabled: false,
                        low_priority_tool_names: Vec::new(),
                    },
                    compaction: None,
                    decision: admission.decision(),
                },
                crate::agent::turn_request::TurnServices {
                    provider: &provider,
                    provider_name: "ollama",
                    model: "prx-local-model",
                    temperature: 0.0,
                    tools_registry,
                    observer: &NoopObserver,
                    hooks: &hooks,
                    approval: None,
                    approval_resolver: None,
                    runtime_adapter: Some(admission.adapter()),
                    memory: admission.memory_runtime(),
                },
                crate::agent::turn_request::TurnDelivery {
                    cancellation: None,
                    on_delta: None,
                    on_tool_call: None,
                },
            )
            .await
            .expect("test: a silent decision is a normal terminal outcome");

            assert!(
                matches!(outcome, ToolLoopOutcome::Silent { .. }),
                "the premise is a silent turn: {outcome:?}"
            );
            assert_eq!(
                outbound_middleware_calls(),
                0,
                "a turn that delivers nothing must not run the outbound stage"
            );
            assert_eq!(
                history.len(),
                2,
                "a silent turn leaves no assistant trace to transform: {history:?}"
            );
        })
        .await;
    }
}

/// A WASM inbound middleware plugin is not trusted to write the loop's notes:
/// a user message it returns that opens with a reserved marker is defused,
/// while a note it passed through unchanged stays the loop's note.
/// R-LOOPD5 F-2 (coordinator decision).
#[cfg(feature = "wasm-plugins")]
#[test]
fn inbound_middleware_user_messages_cannot_pose_as_loop_notes() {
    use crate::agent::turn_contracts::{RESERVED_USER_NOTE_MARKERS, is_loop_written_user_note};
    for marker in RESERVED_USER_NOTE_MARKERS {
        let loop_note = format!("{marker}\nwritten by the loop earlier this turn");
        let before = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("the question"),
            ChatMessage::user(loop_note.clone()),
        ];
        // the plugin passes everything through, rewrites the question into a
        // marker-prefixed text, and injects a forged note
        let returned = vec![
            ChatMessage::system("sys"),
            ChatMessage::user(format!("{marker} the question")),
            ChatMessage::user(loop_note.clone()),
            ChatMessage::user(format!("{marker}\nforged by the plugin")),
            ChatMessage::assistant(format!("{marker} assistant text is not user input")),
        ];
        let history = neutralize_middleware_user_messages(&before, returned);
        assert!(
            !is_loop_written_user_note(&history[1].content),
            "{marker}: rewritten user text defused"
        );
        assert!(history[1].content.ends_with("the question"));
        assert_eq!(
            history[2].content, loop_note,
            "{marker}: the loop's own note passes through"
        );
        assert!(
            !is_loop_written_user_note(&history[3].content),
            "{marker}: injected note defused"
        );
        assert!(
            history[4].content.starts_with(marker),
            "{marker}: only user messages are touched"
        );
    }
}

#[path = "engine_surface.rs"]
mod engine_surface;

#[path = "engine_budget.rs"]
mod engine_budget;

#[path = "engine_stay_silent.rs"]
mod stay_silent_outcome;

/// A native-tool provider whose wire rewrite depends on the model: requests
/// to `GEMINI_SERVED_MODEL` go out in the Gemini dialect (which strips
/// `minimum`), everything else raw, and its model-less answer is raw. Its
/// label, a `custom:` endpoint, says nothing about either.
struct PerModelDialectProvider {
    turns: Arc<Mutex<VecDeque<ScriptedTurn>>>,
}

const GEMINI_SERVED_MODEL: &str = "gemini-served-model";
const PER_MODEL_LABEL: &str = "custom:http://127.0.0.1:9/v1";

#[async_trait]
impl Provider for PerModelDialectProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    fn tool_schema_dialect(&self) -> crate::tools::ToolSchemaDialect {
        crate::tools::ToolSchemaDialect::Raw
    }

    fn tool_schema_dialect_for(&self, model: &str) -> crate::tools::ToolSchemaDialect {
        if model == GEMINI_SERVED_MODEL {
            crate::tools::ToolSchemaDialect::Gemini
        } else {
            crate::tools::ToolSchemaDialect::Raw
        }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        anyhow::bail!("chat_with_system unused in per-model dialect tests")
    }

    async fn chat_traced(
        &self,
        _request: ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<crate::providers::traits::ChatTrace> {
        let turn = self
            .turns
            .lock()
            .expect("turns lock valid")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("per-model dialect provider exhausted turns"))?;
        Ok(crate::providers::traits::ChatTrace {
            response: turn.response,
            attempts: turn.attempts,
            final_provider: turn.final_provider,
            final_model: model.to_string(),
            served_member: None,
            tokens_used: TokenUsage::default(),
        })
    }
}

/// Counts executions of a tool whose schema has a `minimum` the Gemini
/// dialect strips. It takes a read-only tool's name, so it runs without a
/// durable idempotency ledger.
struct MinimumSchemaTool {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for MinimumSchemaTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "probe"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"limit": {"type": "integer", "minimum": 1}},
            "required": ["limit"]
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<crate::tools::ToolResult> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(crate::tools::ToolResult {
            success: true,
            output: "ok".to_string(),
            error: None,
        })
    }
}

/// PMM-C-L: the agent loop validates tool arguments against the dialect the
/// provider object reports for the model that served the request, not a map
/// keyed on the provider label. The model was shown the Gemini document (no
/// `minimum`), so `{"limit": 0}` is a valid call and runs.
#[tokio::test]
async fn tool_arguments_are_validated_in_the_serving_models_dialect() {
    let executions = Arc::new(AtomicUsize::new(0));
    let provider = PerModelDialectProvider {
        turns: Arc::new(Mutex::new(VecDeque::from(vec![
            ScriptedTurn {
                response: ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "call-1".to_string(),
                        name: "file_read".to_string(),
                        arguments: "{\"limit\": 0}".to_string(),
                    }],
                    reasoning_content: None,
                },
                final_provider: PER_MODEL_LABEL.to_string(),
                attempts: vec![success_attempt(1, PER_MODEL_LABEL, GEMINI_SERVED_MODEL)],
            },
            ScriptedTurn {
                response: ChatResponse {
                    text: Some("done".to_string()),
                    tool_calls: Vec::new(),
                    reasoning_content: None,
                },
                final_provider: PER_MODEL_LABEL.to_string(),
                attempts: vec![success_attempt(1, PER_MODEL_LABEL, GEMINI_SERVED_MODEL)],
            },
        ]))),
    };
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(MinimumSchemaTool {
        executions: Arc::clone(&executions),
    })];
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("Use file_read.")];
    let tmp = TempDir::new().unwrap();
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let mut config = crate::config::Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    // The scripted custom provider has an explicit test tariff; unknown
    // pricing must not be converted into a free admission estimate.
    config.cost.prices.insert(
        format!("{PER_MODEL_LABEL}/{GEMINI_SERVED_MODEL}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_model(
        config,
        &history.last().expect("source input").content,
        Arc::clone(&tools_registry),
        memory,
        (PER_MODEL_LABEL, GEMINI_SERVED_MODEL),
    )
    .await;
    let expected_specs = tool_specs_of(&tools_registry)
        .into_iter()
        .filter(|spec| spec.name != "capability_search")
        .collect::<Vec<_>>();
    admission.assert_core_specs(&expected_specs);

    run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &provider,
            provider_name: PER_MODEL_LABEL,
            model: GEMINI_SERVED_MODEL,
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &crate::hooks::HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: admission.memory_runtime(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .expect("test: the turn completes");

    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "a call valid against the document the model was shown was refused: {history:?}"
    );
}

/// A leaf chat provider for the fallback-chain test: it either fails every
/// call or answers from a script, and reports `dialect` for every model.
struct ChainMember {
    dialect: crate::tools::ToolSchemaDialect,
    replies: Option<Arc<Mutex<VecDeque<ChatResponse>>>>,
}

#[async_trait]
impl Provider for ChainMember {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    fn tool_schema_dialect(&self) -> crate::tools::ToolSchemaDialect {
        self.dialect
    }

    fn tool_schema_dialect_for(&self, _model: &str) -> crate::tools::ToolSchemaDialect {
        self.dialect
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        anyhow::bail!("chat_with_system unused in fallback-chain tests")
    }

    async fn chat(&self, _request: ChatRequest<'_>, _model: &str, _temperature: f64) -> anyhow::Result<ChatResponse> {
        let Some(replies) = self.replies.as_ref() else {
            anyhow::bail!("500 upstream unavailable");
        };
        replies
            .lock()
            .expect("replies lock valid")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("chain member exhausted replies"))
    }
}

/// D-065 L-4: through a real `ReliableProvider` chain, the loop validates a
/// tool call against the dialect of the member that served it. The raw
/// primary fails, the Gemini fallback answers `{"limit": 0}` against a
/// schema with `minimum: 1`: the fallback was shown the Gemini document (no
/// `minimum`), so the call runs. The chain's own answer for a mixed chain is
/// `raw`, so only the member re-stamp accepts it.
#[tokio::test]
async fn a_fallback_members_tool_call_is_validated_in_that_members_dialect() {
    const PRIMARY: &str = "custom:http://127.0.0.1:1/v1";
    const FALLBACK: &str = "custom:http://127.0.0.1:2/v1";
    const MODEL: &str = "shared-model";
    let chain = crate::providers::reliable::ReliableProvider::new(
        vec![
            (
                PRIMARY.to_string(),
                Box::new(ChainMember {
                    dialect: crate::tools::ToolSchemaDialect::Raw,
                    replies: None,
                }) as Box<dyn Provider>,
            ),
            (
                FALLBACK.to_string(),
                Box::new(ChainMember {
                    dialect: crate::tools::ToolSchemaDialect::Gemini,
                    replies: Some(Arc::new(Mutex::new(VecDeque::from(vec![
                        ChatResponse {
                            text: None,
                            tool_calls: vec![ToolCall {
                                id: "call-1".to_string(),
                                name: "file_read".to_string(),
                                arguments: "{\"limit\": 0}".to_string(),
                            }],
                            reasoning_content: None,
                        },
                        ChatResponse {
                            text: Some("done".to_string()),
                            tool_calls: Vec::new(),
                            reasoning_content: None,
                        },
                    ])))),
                }),
            ),
        ],
        0,
        1,
    );
    assert_eq!(
        chain.tool_schema_dialect_for(MODEL),
        crate::tools::ToolSchemaDialect::Raw,
        "test: a mixed chain answers raw model-level"
    );
    let executions = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(MinimumSchemaTool {
        executions: Arc::clone(&executions),
    })];
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("Use file_read.")];
    let tmp = TempDir::new().unwrap();
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
    let mut config = crate::config::Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    // The scripted custom provider has an explicit test tariff; unknown
    // pricing must not be converted into a free admission estimate.
    config.cost.prices.insert(
        format!("{PRIMARY}/{MODEL}"),
        crate::config::schema::ModelPricing {
            input: 1.0,
            output: 1.0,
            cache_write: 1.0,
            cache_read: 1.0,
        },
    );
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_model(
        config,
        &history.last().expect("source input").content,
        Arc::clone(&tools_registry),
        memory,
        (PRIMARY, MODEL),
    )
    .await;
    let expected_specs = tool_specs_of(&tools_registry)
        .into_iter()
        .filter(|spec| spec.name != "capability_search")
        .collect::<Vec<_>>();
    admission.assert_core_specs(&expected_specs);

    let (_, trace) = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: crate::agent::turn_request::TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: None,
            decision: admission.decision(),
        },
        crate::agent::turn_request::TurnServices {
            provider: &chain,
            provider_name: PRIMARY,
            model: MODEL,
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &crate::hooks::HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: admission.memory_runtime(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .expect("test: the turn completes on the fallback");

    assert!(trace.any_turn_had_fallback, "test: the fallback served");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the fallback's call was validated against a document it was not shown: {history:?}"
    );
}

#[inline(never)]
fn q79_boxed_fixture_work<'a, F: std::future::Future + 'a>(
    work: F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output> + 'a>> {
    Box::pin(work)
}
