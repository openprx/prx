#![cfg(test)]
//! The newest validated context handoff survives every front-first trim of
//! a provider view while any other removable message remains, and a view
//! without a handoff trims exactly as before (REQ-HANDOFF-RECALL-AND-TRIM M2).

use super::*;

/// The fixture messages at `indices`, in order: the expected trim output
/// stated as positions in the untrimmed fixture.
fn kept(original: &[ChatMessage], indices: &[usize]) -> Vec<ChatMessage> {
    indices
        .iter()
        .map(|index| {
            original
                .get(*index)
                .cloned()
                .expect("test: golden index inside the fixture")
        })
        .collect()
}

/// A window whose usable input is exactly the estimate of `fits`.
fn window_holding(fits: &[ChatMessage]) -> crate::config::AgentCompactionConfig {
    crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Off,
        reserve_tokens: 1,
        keep_recent_messages: 2,
        max_context_tokens: estimate_history_tokens(fits).saturating_add(1),
        max_context_tokens_explicit: true,
        ..crate::config::AgentCompactionConfig::default()
    }
}

/// `history` with every message from `from` on, behind its system prompt.
fn system_and_tail(history: &[ChatMessage], from: usize) -> Vec<ChatMessage> {
    history
        .iter()
        .take(1)
        .chain(history.iter().skip(from))
        .cloned()
        .collect()
}

/// Golden outputs of the three trim helpers on views without a handoff,
/// recorded from the trims as they were before the handoff protection.
#[test]
fn a_view_without_a_handoff_trims_exactly_as_before() {
    // 1. Token-aware budget trim: the oldest messages go until the view fits.
    let fixture = crate::agent::turn_tests::tool_roundtrip_history_fixture(10);
    let config = window_holding(&system_and_tail(&fixture, 29));
    let mut view = fixture.clone();
    assert!(trim_history_to_context_budget(&mut view, &config));
    let mut expected_indices = vec![0];
    expected_indices.extend(29..=40);
    assert!(
        crate::agent::turn_history::same_messages(&view, &kept(&fixture, &expected_indices)),
        "the budget trim keeps the system prompt and rounds 7 to 9"
    );

    // 2. The trim that preserves a compaction replacement (a summary here).
    let mut with_summary = vec![
        ChatMessage::system("sys"),
        ChatMessage::assistant(format!("summary {}", "decided ".repeat(30))),
    ];
    with_summary.extend(fixture.iter().skip(1).cloned());
    let fits: Vec<ChatMessage> = with_summary
        .iter()
        .take(2)
        .chain(with_summary.iter().skip(34))
        .cloned()
        .collect();
    let config = window_holding(&fits);
    let mut view = with_summary.clone();
    assert!(trim_history_to_context_budget_preserving_compaction_replacement_with_floor(&mut view, &config, 1));
    let mut expected_indices = vec![0, 1];
    expected_indices.extend(34..=41);
    assert!(
        crate::agent::turn_history::same_messages(&view, &kept(&with_summary, &expected_indices)),
        "the preserving trim keeps the summary and rounds 8 and 9"
    );

    // 3. The count-based aggressive trim, whose cut strands a tool result.
    let mut view = fixture.clone();
    apply_aggressive_trim(&mut view, 6);
    assert!(
        crate::agent::turn_history::same_messages(&view, &kept(&fixture, &[0, 36, 37, 38, 39, 40])),
        "the aggressive trim keeps six messages and drops the stranded tool result"
    );
}

/// Bound of one engine-level test: its scripted provider, its real SQLite
/// store and its tools.
const HANDOFF_TRIM_TEST_DEADLINE: Duration = Duration::from_secs(120);

/// A validated handoff note referencing three stored rows, as stored.
fn stored_handoff(id: &str) -> ChatMessage {
    let message = ChatMessage::assistant(crate::agent::compaction::handoff_message_for_test(
        id,
        &["row-a", "row-b", "row-c"],
        1,
    ));
    assert!(
        crate::agent::compaction::is_validated_context_handoff_message(&message),
        "premise: the stored fixture handoff validates"
    );
    message
}

/// The compact form a chat view shows `stored` in, made by the one renderer.
fn rendered_handoff(stored: &ChatMessage) -> ChatMessage {
    let rendered = ChatMessage::assistant(
        crate::agent::compaction::render_context_handoff_for_provider(&stored.role, &stored.content).into_owned(),
    );
    assert!(
        rendered.content != stored.content && rendered.content.starts_with("[context_handoff] handoff_event_id="),
        "premise: the renderer made the compact form"
    );
    assert!(
        crate::agent::compaction::is_validated_context_handoff_message(&rendered),
        "premise: the compact rendering is a protected handoff"
    );
    rendered
}

/// Both forms a provider view holds a handoff in: the stored note (a caller
/// without an acknowledged session) and its compact rendering (a chat view).
fn handoff_forms(id: &str) -> [(&'static str, ChatMessage); 2] {
    let stored = stored_handoff(id);
    let rendered = rendered_handoff(&stored);
    [("stored note", stored), ("compact rendering", rendered)]
}

/// `fixture` with `inserted` right behind its system prompt.
fn behind_system(fixture: &[ChatMessage], inserted: ChatMessage) -> Vec<ChatMessage> {
    let mut view = fixture.to_vec();
    view.insert(1, inserted);
    view
}

/// Only an assistant message holding a validated stored note or exactly its
/// compact rendering is protected; anything else that opens with the marker
/// is not.
#[test]
fn only_a_validated_note_or_its_exact_rendering_is_protected() {
    let stored = stored_handoff("handoff-shape");
    let rendered = rendered_handoff(&stored);
    let folded = format!("{} 2 tool round-trips were folded.", rendered.content);
    let not_protected = [
        ChatMessage::assistant("[context_handoff] not a note"),
        ChatMessage::assistant(format!("{} And something more.", rendered.content)),
        ChatMessage::assistant(rendered.content.replace("handoff-shape —", "handoff shape —")),
        ChatMessage::assistant(rendered.content.replace("— 3 earlier", "— 0 earlier")),
        ChatMessage::assistant(rendered.content.replace("— 3 earlier", "— +3 earlier")),
        ChatMessage::assistant(format!("{} 0 tool round-trips were folded.", rendered.content)),
        ChatMessage::user(rendered.content),
        ChatMessage::user(stored.content),
    ];
    for message in &not_protected {
        assert!(
            !crate::agent::compaction::is_validated_context_handoff_message(message),
            "not a protected handoff: {}: {}",
            message.role,
            message.content
        );
    }
    assert!(crate::agent::compaction::is_validated_context_handoff_message(
        &ChatMessage::assistant(folded)
    ));
}

/// The shared trim helpers keep the newest validated handoff, in either form,
/// while any other removable message is older than the view's newest one, and
/// remove it last when nothing else is left. An older handoff and a message
/// that only looks like a handoff are trimmed as any other message.
#[test]
fn newest_handoff_is_trimmed_last_on_context_budget_trim() {
    let fixture = crate::agent::turn_tests::tool_roundtrip_history_fixture(10);
    for (form, newest) in handoff_forms("handoff-newest") {
        let view = behind_system(&fixture, newest.clone());
        // [sys, handoff, fixture 1..=40]: fixture message i sits at i + 1.
        let mut expected_indices = vec![0, 1];
        expected_indices.extend(30..=41);
        let expected = kept(&view, &expected_indices);
        let config = window_holding(&expected);
        let mut trimmed = view.clone();
        trim_history_to_context_budget(&mut trimmed, &config);
        assert!(
            crate::agent::turn_history::same_messages(&trimmed, &expected),
            "{form}, token-aware: the handoff stays and the trim takes the oldest rounds instead"
        );
        let mut trimmed = view.clone();
        trim_history_to_context_budget_preserving_compaction_replacement_with_floor(&mut trimmed, &config, 0);
        assert!(
            crate::agent::turn_history::same_messages(&trimmed, &expected),
            "{form}, preserving without a replacement: the handoff stays"
        );

        // Only the handoff and the newest message would be left: the handoff
        // goes, as before, and the trim ends without an error.
        let newest_only = kept(&view, &[0, 41]);
        let mut trimmed = view.clone();
        trim_history_to_context_budget(&mut trimmed, &window_holding(&newest_only));
        assert!(
            crate::agent::turn_history::same_messages(&trimmed, &newest_only),
            "{form}: a window too small for the handoff and the newest message drops the handoff last"
        );

        // Count-based: the cut skips the handoff and keeps the same recent tail.
        let mut trimmed = view.clone();
        apply_aggressive_trim(&mut trimmed, 6);
        assert!(
            crate::agent::turn_history::same_messages(&trimmed, &kept(&view, &[0, 1, 37, 38, 39, 40, 41])),
            "{form}: the aggressive trim keeps the handoff and drops the stranded tool result"
        );
        let mut trimmed = view;
        apply_aggressive_trim(&mut trimmed, 1);
        assert!(
            crate::agent::turn_history::same_messages(&trimmed, &newest_only),
            "{form}: keeping one message keeps the newest one, not the handoff"
        );

        // Two handoffs and look-alikes: only the newest validated one is kept.
        let mut two = behind_system(&fixture, ChatMessage::assistant("[context_handoff] not a note"));
        two.insert(1, stored_handoff("handoff-older"));
        two.insert(2, ChatMessage::user(newest.content.clone()));
        two.insert(4, newest);
        // [sys, older, user copy, look-alike, newest, fixture 1..=40]: fixture i sits at i + 4.
        let mut expected_indices = vec![0, 4];
        expected_indices.extend(33..=44);
        let expected = kept(&two, &expected_indices);
        let mut trimmed = two.clone();
        trim_history_to_context_budget(&mut trimmed, &window_holding(&expected));
        assert!(
            crate::agent::turn_history::same_messages(&trimmed, &expected),
            "{form}: the older handoff, the user copy and the look-alike are trimmed; the newest handoff stays"
        );
    }
}

/// Serves a script of responses (an `Err` entry is a provider error) and
/// records every request's messages.
struct ScriptedRequestLog {
    native_tools: bool,
    responses: Mutex<VecDeque<Result<ChatResponse, String>>>,
    requests: Mutex<Vec<Vec<ChatMessage>>>,
}

impl ScriptedRequestLog {
    fn new(native_tools: bool, responses: Vec<Result<ChatResponse, String>>) -> Self {
        Self {
            native_tools,
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Vec<ChatMessage>> {
        self.requests.lock().expect("test: request log lock").clone()
    }
}

#[async_trait]
impl Provider for ScriptedRequestLog {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: self.native_tools,
            vision: false,
        }
    }

    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("test: no summarizer runs in these fixtures")
    }

    async fn chat(&self, request: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        self.requests
            .lock()
            .expect("test: request log lock")
            .push(request.messages.to_vec());
        let next = self
            .responses
            .lock()
            .expect("test: response script lock")
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("test: the scripted provider ran out of responses"))?;
        next.map_err(|error| anyhow::anyhow!(error))
    }
}

fn text(reply: &str) -> Result<ChatResponse, String> {
    Ok(ChatResponse {
        text: Some(reply.to_string()),
        tool_calls: Vec::new(),
        reasoning_content: None,
    })
}

fn native_call(id: &str, name: &str, arguments: &str) -> Result<ChatResponse, String> {
    Ok(ChatResponse {
        text: None,
        tool_calls: vec![ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
        }],
        reasoning_content: None,
    })
}

/// Run one turn of the real loop over `history`.
async fn drive_turn(
    history: &mut Vec<ChatMessage>,
    provider: &ScriptedRequestLog,
    admission: &crate::agent::turn_tests::admission::AdmittedTurn,
    compaction: Option<&crate::config::AgentCompactionConfig>,
    tools_registry: Arc<Vec<Box<dyn Tool>>>,
    adapter: crate::agent::turn_contracts::ToolLoopRuntimeAdapter,
    memory: ToolLoopMemory,
    workspace: &std::path::Path,
) -> anyhow::Result<String> {
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
            compaction,
            decision: admission.decision(),
        },
        TurnServices {
            provider,
            provider_name: "ollama",
            model: "prx-local-model",
            temperature: 0.0,
            tools_registry,
            observer: &NoopObserver,
            hooks: &HookManager::new(workspace.to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(adapter),
            memory,
        },
        TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .map(|(outcome, _trace)| outcome.into_text())
}

/// Whether `request` shows the model `handoff` byte for byte, in the form the
/// turn's view holds it.
fn carries(request: &[ChatMessage], handoff: &ChatMessage) -> bool {
    request
        .iter()
        .any(|sent| sent.role == handoff.role && sent.content == handoff.content)
}

/// The turn memory for a view holding the handoff in `form`: a chat view (the
/// compact rendering) comes with its canonical history, which holds the stored
/// note in its place; a caller without a session supplies none, its view is
/// its history.
fn form_memory(memory: ToolLoopMemory, form: &str, view: &[ChatMessage], stored: &ChatMessage) -> ToolLoopMemory {
    if form == "stored note" {
        return memory;
    }
    let canonical = view
        .iter()
        .map(|message| {
            if crate::agent::compaction::is_validated_context_handoff_message(message) {
                stored.clone()
            } else {
                message.clone()
            }
        })
        .collect();
    memory.with_canonical_history(
        crate::agent::turn_history::CanonicalHistory::from_test_messages(canonical),
        false,
    )
}

/// The per-request preflight of the turn engine trims an over-budget view
/// before the first provider call; the handoff behind the system prompt
/// stays while older turns go.
#[tokio::test]
async fn newest_handoff_is_trimmed_last_on_preflight_projection() {
    tokio::time::timeout(
        HANDOFF_TRIM_TEST_DEADLINE,
        Box::pin(async {
            for (form, note) in handoff_forms("handoff-preflight") {
                let stored = stored_handoff("handoff-preflight");
                let shown = note.clone();
                const OLDEST_TURN: &str = "preflight-oldest-turn ";
                let mut history = vec![ChatMessage::system("sys"), note];
                for turn in 0..23 {
                    let marker = if turn == 0 { OLDEST_TURN } else { "turn " };
                    history.push(ChatMessage::user(format!(
                        "{marker}{turn} {}",
                        "long context ".repeat(400)
                    )));
                }
                history.push(ChatMessage::user("current preflight request"));
                let config = crate::config::AgentCompactionConfig {
                    mode: crate::config::AgentCompactionMode::Off,
                    reserve_tokens: 10,
                    keep_recent_messages: 2,
                    max_context_tokens: 4106,
                    max_context_tokens_explicit: true,
                    ..crate::config::AgentCompactionConfig::default()
                };
                let workspace = TempDir::new().expect("test: workspace");
                let mut admission_config = crate::config::Config::default();
                admission_config.workspace_dir = workspace.path().to_path_buf();
                admission_config.agent.compaction = config;
                let tools_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
                let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_config(
                    admission_config,
                    "current preflight request",
                    Arc::clone(&tools_registry),
                )
                .await;
                assert!(
                    plan_context_budget(&history, admission.compaction(), PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
                    "premise: the turn starts over its window"
                );
                let provider = ScriptedRequestLog::new(false, vec![text("done")]);

                let memory = form_memory(ToolLoopMemory::none(), form, &history, &stored);
                let reply = drive_turn(
                    &mut history,
                    &provider,
                    &admission,
                    Some(admission.compaction()),
                    tools_registry,
                    admission.adapter(),
                    memory,
                    workspace.path(),
                )
                .await
                .expect("test: the turn completes");

                assert_eq!(reply, "done");
                let requests = provider.requests();
                let first = requests.first().expect("test: the first request was recorded");
                assert!(
                    !first.iter().any(|message| message.content.starts_with(OLDEST_TURN)),
                    "premise: the preflight trimmed the oldest turn"
                );
                assert!(carries(first, &shown), "{form}: the preflight trim keeps the handoff");
                assert!(
                    first
                        .last()
                        .is_some_and(|message| message.content == "current preflight request"),
                    "the request stays last"
                );
            }
        }),
    )
    .await
    .expect("newest_handoff_is_trimmed_last_on_preflight_projection exceeded its deadline");
}

/// The trim that runs after a tool round, before the next iteration: it
/// takes the older exchange and the question (put back afterwards) but not
/// the handoff.
#[tokio::test]
async fn newest_handoff_is_trimmed_last_on_mid_turn_trim() {
    tokio::time::timeout(
        HANDOFF_TRIM_TEST_DEADLINE,
        Box::pin(async {
            for (form, note) in handoff_forms("handoff-mid-turn") {
                let stored = stored_handoff("handoff-mid-turn");
                let shown = note.clone();
                const QUESTION_MARKER: &str = "mid-turn-handoff-request-marker";
                const OLDER_TURN_MARKER: &str = "mid-turn-handoff-older-turn-marker";
                let tools_registry =
                    crate::agent::turn_tests::admission::registry_with_search(vec![Box::new(NamedLargeOutputTool {
                        name: "file_read".to_string(),
                        output: "mid turn tool output ".repeat(8),
                    })]);
                let tool_specs = business_tool_specs_of(&tools_registry);
                let tool_surface = crate::agent::turn_context::measure_request_tool_surface(true, &tool_specs);
                let question = format!("{QUESTION_MARKER} Use file_read. {}", "question-bulk ".repeat(1600));
                let mut history = vec![
                    ChatMessage::system("stable base system prompt"),
                    note,
                    ChatMessage::user(format!("{OLDER_TURN_MARKER} settled earlier")),
                    ChatMessage::assistant("settled"),
                    ChatMessage::user(question.clone()),
                ];
                // A window that holds this history and almost nothing more once the
                // tool surface and the decision engine took their share.
                let main_window = budgeted_tokens(estimate_history_tokens_with_source(&history))
                    .saturating_add(8)
                    .saturating_add(budgeted_request_tool_tokens(tool_surface));
                let mut available = main_window;
                while available - (available / 8).min(8192) < main_window {
                    available = available.saturating_add(1);
                }
                let config = crate::config::AgentCompactionConfig {
                    mode: crate::config::AgentCompactionMode::Off,
                    reserve_tokens: CONTEXT_BUDGET_FIXTURE_RESERVE_TOKENS,
                    keep_recent_messages: 2,
                    max_context_tokens: available.saturating_add(CONTEXT_BUDGET_FIXTURE_RESERVE_TOKENS),
                    max_context_tokens_explicit: true,
                    ..crate::config::AgentCompactionConfig::default()
                };
                let provider =
                    ScriptedRequestLog::new(true, vec![native_call("call-1", "file_read", "{}"), text("done")]);
                let workspace = TempDir::new().expect("test: workspace");
                let memory: Arc<dyn Memory> =
                    Arc::new(SqliteMemory::new(workspace.path()).expect("test: sqlite memory"));
                let mut admission_config = crate::config::Config::default();
                admission_config.workspace_dir = workspace.path().to_path_buf();
                admission_config.agent.compaction = config;
                let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_memory(
                    admission_config,
                    &question,
                    Arc::clone(&tools_registry),
                    Arc::clone(&memory),
                )
                .await;
                let reserved = reserve_request_tool_surface(admission.compaction(), tool_surface);
                assert!(
                    !plan_context_budget(&history, &reserved, PRE_TURN_FLUSH_THRESHOLD).over_hard_limit,
                    "premise: the first iteration fits, so only the mid-turn trim can shrink the view"
                );

                let memory = form_memory(admission.memory_runtime(), form, &history, &stored);
                let reply = drive_turn(
                    &mut history,
                    &provider,
                    &admission,
                    Some(admission.compaction()),
                    tools_registry,
                    admission.adapter(),
                    memory,
                    workspace.path(),
                )
                .await
                .expect("test: the turn completes");

                assert_eq!(reply, "done");
                let requests = provider.requests();
                assert_eq!(requests.len(), 2, "the fixture must reach a second iteration");
                let first = requests.first().expect("test: the first request was recorded");
                assert!(carries(first, &shown), "premise: the first request carried the handoff");
                let second = requests.get(1).expect("test: the second request was recorded");
                assert!(
                    !second.iter().any(|message| message.content.contains(OLDER_TURN_MARKER)),
                    "premise: the mid-turn trim ran and took the older exchange"
                );
                assert!(carries(second, &shown), "{form}: the mid-turn trim keeps the handoff");
                assert!(
                    second.iter().any(|message| message.content.contains(QUESTION_MARKER)),
                    "the question is still put back"
                );
                assert_eq!(first_orphan_tool_index(second), None, "no tool result is stranded");
            }
        }),
    )
    .await
    .expect("newest_handoff_is_trimmed_last_on_mid_turn_trim exceeded its deadline");
}

/// The provider overflow retry when no handoff can be built (compaction
/// off): the count-based trim takes the older turns, not the handoff.
#[tokio::test]
async fn newest_handoff_is_trimmed_last_on_overflow_without_handoff() {
    tokio::time::timeout(
        HANDOFF_TRIM_TEST_DEADLINE,
        Box::pin(async {
            for (form, note) in handoff_forms("handoff-overflow") {
                let stored = stored_handoff("handoff-overflow");
                let shown = note.clone();
                const REQUEST: &str = "Use file_read to read overflow-probe.txt nine times, then report it.";
                const ROUNDS: usize = 9;
                let workspace = TempDir::new().expect("test: workspace");
                std::fs::write(workspace.path().join("overflow-probe.txt"), "probe").expect("test: probe file");
                let security = Arc::new(crate::security::SecurityPolicy::from_config(
                    &crate::config::AutonomyConfig::default(),
                    workspace.path(),
                ));
                let tools_registry = crate::agent::turn_tests::admission::registry_with_search(vec![Box::new(
                    crate::tools::FileReadTool::new(security, false),
                )]);
                let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
                    workspace.path(),
                    REQUEST,
                    Arc::clone(&tools_registry),
                )
                .await;
                let mut adapter = admission.adapter();
                adapter.stream_provider = false;
                let mut script: Vec<Result<ChatResponse, String>> = (0..ROUNDS)
                    .map(|round| {
                        native_call(
                            &format!("overflow-read-{round}"),
                            "file_read",
                            r#"{"path":"overflow-probe.txt"}"#,
                        )
                    })
                    .collect();
                script.push(Err("context window exceeded by scripted provider".to_string()));
                script.push(text("probe complete"));
                let provider = ScriptedRequestLog::new(true, script);
                let mut history = vec![
                    ChatMessage::system("sys"),
                    note,
                    ChatMessage::user("an older request"),
                    ChatMessage::assistant("older completed turn"),
                    ChatMessage::user(REQUEST),
                    ChatMessage::assistant("Current task acknowledged."),
                ];
                let config = crate::config::AgentCompactionConfig {
                    mode: crate::config::AgentCompactionMode::Off,
                    ..crate::config::AgentCompactionConfig::default()
                };

                let memory = form_memory(ToolLoopMemory::none(), form, &history, &stored);
                let reply = drive_turn(
                    &mut history,
                    &provider,
                    &admission,
                    Some(&config),
                    tools_registry,
                    adapter,
                    memory,
                    workspace.path(),
                )
                .await
                .expect("test: the overflow retry completes");

                assert_eq!(reply, "probe complete");
                let requests = provider.requests();
                assert_eq!(
                    requests.len(),
                    ROUNDS + 2,
                    "the tool rounds, the overflow and the retry"
                );
                let overflowed = requests.get(ROUNDS).expect("test: the overflowed request");
                assert!(
                    carries(overflowed, &shown),
                    "premise: the overflowed request carried the handoff"
                );
                let retry = requests.get(ROUNDS + 1).expect("test: the retry request");
                assert!(
                    !retry.iter().any(|message| message.content == "older completed turn"),
                    "premise: the overflow trim took the older turn"
                );
                assert!(carries(retry, &shown), "{form}: the overflow trim keeps the handoff");
                assert_eq!(
                    retry
                        .iter()
                        .filter(|message| message.role == "user" && message.content == REQUEST)
                        .count(),
                    1,
                    "the retry carries the request exactly once"
                );
                assert_eq!(first_orphan_tool_index(retry), None, "no tool result is stranded");
            }
        }),
    )
    .await
    .expect("newest_handoff_is_trimmed_last_on_overflow_without_handoff exceeded its deadline");
}
