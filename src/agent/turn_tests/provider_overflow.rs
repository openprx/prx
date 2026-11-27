#![cfg(test)]

use super::*;
use crate::agent::turn_contracts::{ContextCapacityError, ToolLoopEventSink};
use crate::config::{AgentCompactionConfig, AgentCompactionMode};
use crate::providers::traits::{StreamChunk, StreamError, StreamOptions, StreamResult, ToolCallChunk};
use futures_util::{StreamExt, stream};

const REQUEST: &str = "Use file_read to read overflow-probe.txt nine times, then report its exact contents.";
const PROBE_CONTENT: &str = "prx overflow probe content";

struct OverflowProvider {
    calls: AtomicUsize,
    compaction_calls: AtomicUsize,
    rounds: usize,
    always_overflow: bool,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
    summary_sizes: Mutex<VecDeque<usize>>,
}

impl OverflowProvider {
    fn response(&self, messages: &[ChatMessage]) -> anyhow::Result<ChatResponse> {
        self.requests.lock().expect("captured requests").push(messages.to_vec());
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < self.rounds {
            return Ok(ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: format!("overflow-read-{call}"),
                    name: "file_read".into(),
                    arguments: r#"{"path":"overflow-probe.txt"}"#.into(),
                }],
                reasoning_content: None,
            });
        }
        if self.always_overflow || call == self.rounds {
            anyhow::bail!("context window exceeded by scripted provider");
        }
        Ok(ChatResponse {
            text: Some("probe complete".into()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
}

#[async_trait]
impl Provider for OverflowProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: false,
        }
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        self.compaction_calls.fetch_add(1, Ordering::SeqCst);
        let summary_size = self.summary_sizes.lock().expect("summary script").pop_front();
        if let Some(size) = summary_size {
            return Ok(format!(
                "## Decisions\n{}\n## Constraints\nPreserve Q.\n## Pending\nFinish Q.",
                "detail ".repeat(size)
            ));
        }
        anyhow::bail!("scripted compaction failure")
    }

    async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        self.response(request.messages)
    }

    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        _: &str,
        _: f64,
        _: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        let chunks = match self.response(messages) {
            Ok(response) => {
                let mut chunks = Vec::new();
                if let Some(text) = response.text {
                    chunks.push(Ok(StreamChunk::delta(text)));
                }
                if !response.tool_calls.is_empty() {
                    chunks.push(Ok(StreamChunk::tool_call_chunk(
                        response
                            .tool_calls
                            .into_iter()
                            .enumerate()
                            .map(|(index, call)| ToolCallChunk::new(call.id, call.name, call.arguments, index))
                            .collect(),
                    )));
                }
                chunks.push(Ok(StreamChunk::final_chunk()));
                chunks
            }
            Err(error) => vec![Err(StreamError::Provider(error.to_string()))],
        };
        stream::iter(chunks).boxed()
    }
}

#[derive(Default)]
struct OverflowEvents(Mutex<Vec<ToolLoopEvent>>);

#[async_trait]
impl ToolLoopEventSink for OverflowEvents {
    async fn emit(&self, event: ToolLoopEvent) -> anyhow::Result<()> {
        self.0.lock().expect("overflow events").push(event);
        Ok(())
    }
}

async fn overflow_turn(
    history: &mut Vec<ChatMessage>,
    provider: &OverflowProvider,
    config: Option<&AgentCompactionConfig>,
    streaming: bool,
    events: &Arc<OverflowEvents>,
    workspace: &std::path::Path,
) -> anyhow::Result<(ToolLoopOutcome, ToolLoopTrace)> {
    let security = Arc::new(crate::security::SecurityPolicy::from_config(
        &crate::config::AutonomyConfig::default(),
        workspace,
    ));
    let registry = crate::agent::turn_tests::admission::registry_with_search(vec![Box::new(
        crate::tools::FileReadTool::new(security, false),
    )]);
    let admission =
        crate::agent::turn_tests::admission::AdmittedTurn::with_tools(workspace, REQUEST, Arc::clone(&registry)).await;
    let mut adapter = admission.adapter();
    adapter.stream_provider = streaming;
    adapter.events = Some(events.clone());
    // The session authority this turn reports to holds exactly the turn's
    // starting history.
    let canonical = crate::agent::turn_history::CanonicalHistory::from_test_messages(history.clone());
    run_turn(
        history,
        TurnRequest {
            advice: None,
            channel_name: "cli",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: false,
            multimodal: &crate::config::MultimodalConfig::default(),
            schedule: TurnSchedule {
                read_only_tool_concurrency_window: 2,
                priority_scheduling_enabled: false,
                low_priority_tool_names: Vec::new(),
            },
            compaction: config,
            decision: admission.decision(),
        },
        TurnServices {
            provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry: registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(workspace.to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(adapter),
            memory: ToolLoopMemory::none().with_canonical_history(canonical, false),
        },
        TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
}

fn overflow_config(mode: AgentCompactionMode) -> AgentCompactionConfig {
    AgentCompactionConfig {
        mode,
        ..AgentCompactionConfig::default()
    }
}

#[tokio::test]
async fn provider_overflow_retry_keeps_current_request() {
    Box::pin(async {
        for streaming in [false, true] {
            // None: no config; Off: no patch; Safeguard: auxiliary provider
            // error; Aggressive: replacing summary with the Tail restore rule.
            for config in [
                None,
                Some(overflow_config(AgentCompactionMode::Off)),
                Some(overflow_config(AgentCompactionMode::Switch)),
                Some(overflow_config(AgentCompactionMode::Switch)),
            ] {
                let tmp = TempDir::new().expect("overflow workspace");
                std::fs::write(tmp.path().join("overflow-probe.txt"), PROBE_CONTENT).expect("probe file");
                let provider = OverflowProvider {
                    calls: AtomicUsize::new(0),
                    compaction_calls: AtomicUsize::new(0),
                    rounds: 9,
                    always_overflow: false,
                    requests: Mutex::new(Vec::new()),
                    summary_sizes: Mutex::new(VecDeque::new()),
                };
                let events = Arc::new(OverflowEvents::default());
                // Byte-identical older input cannot stand in for the current Q.
                // Plain assistant boundaries put Q inside the summary range;
                // the nine native call/result pairs are produced by the loop.
                let mut history = vec![
                    ChatMessage::system("sys"),
                    ChatMessage::user(REQUEST),
                    ChatMessage::assistant("older completed turn"),
                    ChatMessage::user(REQUEST),
                    ChatMessage::assistant("Current task acknowledged."),
                    ChatMessage::assistant("Retained boundary before the native rounds."),
                ];
                let (outcome, _) =
                    overflow_turn(&mut history, &provider, config.as_ref(), streaming, &events, tmp.path())
                        .await
                        .expect("overflow recovery completes with Q intact");
                assert_eq!(outcome.into_text(), "probe complete");
                assert_eq!(
                    provider.compaction_calls.load(Ordering::SeqCst) > 0,
                    config
                        .as_ref()
                        .is_some_and(|c| matches!(c.mode, AgentCompactionMode::Switch)),
                    "the error route must really invoke the failing compaction provider"
                );
                let requests = provider.requests.lock().expect("requests");
                assert_eq!(requests.len(), 11, "nine tool rounds, overflow, success");
                let before = requests.get(9).expect("captured overflow request");
                let retry = requests.get(10).expect("captured retry request");
                assert_eq!(before.iter().filter(|m| m.role == "tool").count(), 9);
                assert!(
                    before
                        .iter()
                        .filter(|m| m.role == "tool")
                        .all(|m| m.content.contains(PROBE_CONTENT)),
                    "all nine production file reads must succeed"
                );
                assert_eq!(
                    retry
                        .iter()
                        .filter(|m| m.role == "user" && m.content == REQUEST)
                        .count(),
                    1,
                    "the retry must contain Q exactly once: {retry:?}"
                );
                assert!(!retry.iter().any(|m| m.content == "older completed turn"));
                assert!(retry.iter().any(|m| m.role == "tool"), "retain completed work");
                assert_eq!(first_orphan_tool_index(retry), None, "native call/result pairs survive");
                if streaming
                    && config
                        .as_ref()
                        .is_some_and(|c| matches!(c.mode, AgentCompactionMode::Switch))
                {
                    let events = events.0.lock().expect("events");
                    let (patch, source_history) = events
                        .iter()
                        .find_map(|event| match event {
                            ToolLoopEvent::ContextCompactionPatch {
                                patch, source_history, ..
                            } => Some((patch, source_history)),
                            _ => None,
                        })
                        .expect("streaming summary publishes its restored view");
                    // The patch is built from the canonical history and guarded by it;
                    // the provider requests add prompt sections to the system message.
                    let conversation = |messages: &[ChatMessage]| {
                        messages
                            .iter()
                            .filter(|message| message.role != "system")
                            .map(|message| (message.role.clone(), message.content.clone()))
                            .collect::<Vec<_>>()
                    };
                    assert!(
                        crate::agent::compaction::compaction_patch_guard_matches(
                            source_history.messages(),
                            &patch.guard
                        ),
                        "the patch's guard describes its canonical source history"
                    );
                    assert_eq!(
                        conversation(source_history.messages()),
                        conversation(before),
                        "the patch's source is the canonical history the overflowed request was projected from"
                    );
                    let mut replayed = source_history.messages().to_vec();
                    crate::agent::compaction::apply_compaction_patch_exact(&mut replayed, patch)
                        .expect("test: published patch replays on its source");
                    // The summary's refresh aside is a projection of the retry
                    // request, never part of the patch the session adopts.
                    let without_loop_notes = |messages: &[ChatMessage]| {
                        conversation(messages)
                            .into_iter()
                            .filter(|(_, content)| !crate::agent::turn_contracts::is_loop_written_user_note(content))
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(
                        conversation(&replayed),
                        without_loop_notes(retry),
                        "worker patch replays the retry view without its loop notes"
                    );
                    assert_eq!(
                        retry.last().map(|m| m.content.as_str()),
                        Some(REQUEST),
                        "summary restoration uses Tail"
                    );
                }
            }
        }
    })
    .await;
}

#[tokio::test]
async fn provider_overflow_irreducible_request_stops() {
    Box::pin(async {
        for streaming in [false, true] {
            // At six native rounds the 12-message fallback deletes only Q.
            // Restoring it cancels that apparent progress: stop immediately.
            for rounds in [0, 6] {
                let tmp = TempDir::new().expect("irreducible workspace");
                std::fs::write(tmp.path().join("overflow-probe.txt"), PROBE_CONTENT).expect("probe file");
                let provider = OverflowProvider {
                    calls: AtomicUsize::new(0),
                    compaction_calls: AtomicUsize::new(0),
                    rounds,
                    always_overflow: true,
                    requests: Mutex::new(Vec::new()),
                    summary_sizes: Mutex::new(VecDeque::new()),
                };
                let events = Arc::new(OverflowEvents::default());
                let mut history = vec![ChatMessage::system("sys"), ChatMessage::user(REQUEST)];
                let error = overflow_turn(
                    &mut history,
                    &provider,
                    Some(&overflow_config(AgentCompactionMode::Off)),
                    streaming,
                    &events,
                    tmp.path(),
                )
                .await
                .expect_err("irreducible Q must stop");
                let capacity = error
                    .downcast_ref::<ContextCapacityError>()
                    .expect("typed capacity failure");
                assert!(matches!(capacity, ContextCapacityError::NoProgress { provider_context }
                    if provider_context.contains("context window exceeded by scripted provider")));
                assert_eq!(
                    provider.calls.load(Ordering::SeqCst),
                    rounds + 1,
                    "no progress cannot replay"
                );
                assert_eq!(history.len(), 2 + 2 * rounds);
                assert_eq!(
                    history.get(1).map(|message| message.content.as_str()),
                    Some(REQUEST),
                    "live ask is never truncated"
                );
                assert_eq!(first_orphan_tool_index(&history), None);
            }
        }
    })
    .await;
}

#[tokio::test]
async fn provider_overflow_recovery_stops_after_three_passes() {
    Box::pin(async {
        for streaming in [false, true] {
            let tmp = TempDir::new().expect("pass cap workspace");
            let provider = OverflowProvider {
                calls: AtomicUsize::new(0),
                compaction_calls: AtomicUsize::new(0),
                rounds: 0,
                always_overflow: true,
                requests: Mutex::new(Vec::new()),
                summary_sizes: Mutex::new(VecDeque::from([1_000, 200, 10])),
            };
            let mut history = vec![
                ChatMessage::system("sys"),
                ChatMessage::user(REQUEST),
                ChatMessage::assistant("old detail ".repeat(4_000)),
                ChatMessage::assistant("retained boundary"),
            ];
            let mut config = overflow_config(AgentCompactionMode::Switch);
            config.keep_recent_messages = 1;
            let error = overflow_turn(
                &mut history,
                &provider,
                Some(&config),
                streaming,
                &Arc::new(OverflowEvents::default()),
                tmp.path(),
            )
            .await
            .expect_err("finite recovery cap");
            assert!(
                matches!(
                    error.downcast_ref::<ContextCapacityError>(),
                    Some(ContextCapacityError::RecoveryPassLimit { .. })
                ),
                "{error:#}"
            );
            assert_eq!(provider.compaction_calls.load(Ordering::SeqCst), 3);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 4);
            for request in provider.requests.lock().expect("requests").iter() {
                assert_eq!(
                    request
                        .iter()
                        .filter(|message| message.role == "user" && message.content == REQUEST)
                        .count(),
                    1
                );
            }
        }
    })
    .await;
}
