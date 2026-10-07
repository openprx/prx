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
use crate::tools::StaySilentTool;

async fn run_without_tools(
    responses: Vec<&str>,
) -> (anyhow::Result<(ToolLoopOutcome, ToolLoopTrace)>, Vec<ChatMessage>) {
    let provider = ScriptedProvider::from_text_responses(responses);
    let tools_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
    let tmp = TempDir::new().unwrap();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(tmp.path(), "hi").await;
    let outcome = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "terminal",
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
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await;
    (outcome, history)
}

async fn run_without_tools_configured_answer(
    responses: Vec<&str>,
    workspace: &std::path::Path,
    endpoint: &str,
) -> (anyhow::Result<(ToolLoopOutcome, ToolLoopTrace)>, Vec<ChatMessage>) {
    let provider = ScriptedProvider::from_text_responses(responses);
    let tools_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(Vec::new());
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
    assert!(!provider.capabilities().native_tool_calling);
    assert!(!provider.capabilities().vision);
    let admission =
        crate::agent::turn_tests::admission::AdmittedTurn::configured_answer(workspace, "hi", endpoint).await;
    admission.assert_selected_specs(&[]);
    let outcome = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "terminal",
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
            hooks: &crate::hooks::HookManager::new(workspace.to_path_buf()),
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
    assert!(
        provider.responses.lock().expect("responses lock").is_empty(),
        "both original scripted responses consumed"
    );
    (outcome, history)
}

#[test]
fn tool_summary_only_response_requires_an_exact_standalone_marker() {
    assert!(is_tool_summary_only_response("[Used tools: shell]"));
    assert!(is_tool_summary_only_response(
        " \n[Used tools: shell, web_search_tool]\n"
    ));
    assert!(!is_tool_summary_only_response("[Used tools:]"));
    assert!(!is_tool_summary_only_response(
        "[Used tools: shell]\nThe requested check completed."
    ));
    assert!(!is_tool_summary_only_response("Used tools: shell"));
}

#[tokio::test]
async fn empty_assistant_response_retries_and_returns_real_answer() {
    use futures::FutureExt;
    let workspace = TempDir::new().expect("owned retry workspace").keep();
    eprintln!("owned_retry_workspace={}", workspace.display());
    let driver = crate::agent::agent::native_decision_fixture::Driver::start_answer()
        .await
        .expect("owned actual compatible Answer endpoint");
    let endpoint = driver.endpoint().to_owned();
    let (result, history) = driver
        .run_factory(true, || async {
            let cleanup =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())
                    .expect("actual retry cleanup owner");
            let caught = cleanup
                .run(async {
                    Ok(std::panic::AssertUnwindSafe(run_without_tools_configured_answer(
                        vec!["   \n", "Recovered answer."],
                        &workspace,
                        &endpoint,
                    ))
                    .catch_unwind()
                    .await)
                })
                .await
                .expect("both actual service queues and owner must drain");
            match caught {
                Ok(result) => result,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        })
        .await;
    let (outcome, _trace) = result.expect("empty response corrective retry should complete");

    assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "Recovered answer."));
    assert_eq!(history.len(), 3);
    assert_eq!(
        history.last().map(|message| message.content.as_str()),
        Some("Recovered answer.")
    );
    assert!(
        history
            .iter()
            .all(|message| !message.content.contains(EMPTY_ASSISTANT_RESPONSE_RETRY_MESSAGE)),
        "the corrective instruction must remain request-local: {history:?}"
    );
}

#[tokio::test]
async fn empty_response_retry_keeps_one_leading_system_message_unchanged() {
    let provider = StrictSystemPositionRetryProvider {
        calls: AtomicUsize::new(0),
        first_system: Mutex::new(None),
    };
    let tmp = TempDir::new().expect("admission workspace");
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(tmp.path(), "hi").await;
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
            hooks: &HookManager::new(tmp.path().to_path_buf()),
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
    .expect("strict provider should accept the canonical retry transcript");

    assert!(matches!(result.0, ToolLoopOutcome::Text(ref text) if text == "Recovered answer."));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(
        history
            .iter()
            .all(|message| !message.content.contains(EMPTY_ASSISTANT_RESPONSE_RETRY_MESSAGE)),
        "the corrective instruction must remain request-local: {history:?}"
    );
}

#[tokio::test]
async fn repeated_empty_assistant_response_fails_without_polluting_history() {
    let (result, history) = run_without_tools(vec!["", " \n "]).await;
    let error = result.expect_err("a repeated empty response must fail closed");

    assert!(error.to_string().contains("after one corrective retry"));
    assert_eq!(history.len(), 2);
    assert!(history.iter().all(|message| message.role != "assistant"));
}

#[tokio::test]
async fn tool_summary_only_response_retries_and_returns_real_answer() {
    let (result, history) = run_without_tools(vec!["[Used tools: shell]", "The requested check completed."]).await;
    let (outcome, _trace) = result.expect("corrective retry should complete");

    assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "The requested check completed."));
    assert_eq!(history.len(), 3);
    assert_eq!(history.last().map(|message| message.role.as_str()), Some("assistant"));
    assert_eq!(
        history.last().map(|message| message.content.as_str()),
        Some("The requested check completed.")
    );
    assert!(
        history
            .iter()
            .all(|message| !message.content.contains(TOOL_SUMMARY_ONLY_RESPONSE_MESSAGE)),
        "the corrective instruction must remain request-local: {history:?}"
    );
}

#[tokio::test]
async fn repeated_tool_summary_only_response_fails_without_polluting_history() {
    let (result, history) = run_without_tools(vec!["[Used tools: shell]", "[Used tools: shell]"]).await;
    let error = result.expect_err("a repeated summary-only response must fail closed");

    assert!(error.to_string().contains("after one corrective retry"));
    assert_eq!(history.len(), 2);
    assert!(
        history
            .iter()
            .all(|message| !is_tool_summary_only_response(&message.content)),
        "summary-only artifacts must not enter history: {history:?}"
    );
}

#[test]
fn fabricated_tool_marker_is_recognized_per_line() {
    assert!(contains_fabricated_tool_marker("[Used tools: shell]"));
    assert!(contains_fabricated_tool_marker("Done.\n[Used tools: shell, gateway]"));
    assert!(contains_fabricated_tool_marker("[Used tools: shell]\nDone."));
    assert!(contains_fabricated_tool_marker(
        "Done.\n  [Used tools: shell]  \nAnything else?"
    ));
    // An empty list is not the fabricated shape.
    assert!(!contains_fabricated_tool_marker("Done.\n[Used tools:]"));
    // Neither is the same words without the brackets.
    assert!(!contains_fabricated_tool_marker("Used tools: shell"));
    // Nor a marker glued into a sentence: only whole lines count.
    assert!(!contains_fabricated_tool_marker(
        "I ran [Used tools: shell] on your box."
    ));
}

#[test]
fn strip_fabricated_tool_markers_removes_only_marker_lines() {
    assert_eq!(
        strip_fabricated_tool_markers("[Used tools: shell]\nDone.").as_ref(),
        "Done."
    );
    assert_eq!(
        strip_fabricated_tool_markers("Done.\n[Used tools: shell]").as_ref(),
        "Done."
    );
    assert_eq!(
        strip_fabricated_tool_markers("Head.\n  [Used tools: shell, gateway]  \nTail.").as_ref(),
        "Head.\nTail."
    );
    assert_eq!(
        strip_fabricated_tool_markers("A\n[Used tools: shell]\nB\n[Used tools: gateway]\nC").as_ref(),
        "A\nB\nC"
    );
    // Whole response is the marker: stripping leaves nothing, which the
    // caller turns into the existing summary-only failure.
    assert!(strip_fabricated_tool_markers("[Used tools: shell]").as_ref().is_empty());
}

#[test]
fn strip_fabricated_tool_markers_borrows_when_there_is_nothing_to_strip() {
    for untouched in [
        "Plain answer.",
        "[Used tools:]",
        "Used tools: shell",
        "I ran [Used tools: shell] on your box.",
        "See [1] and [note: unrelated] plus [Used tools] alone.",
        "[Used tools: shell\nstill going]",
    ] {
        let result = strip_fabricated_tool_markers(untouched);
        assert!(
            matches!(result, Cow::Borrowed(_)),
            "a text without a marker line must not allocate: {untouched:?}"
        );
        assert_eq!(result.as_ref(), untouched);
    }
}

#[tokio::test]
async fn fabricated_tool_marker_without_tool_execution_retries_once() {
    let provider = ScriptedProvider::from_text_responses(vec![
        "The disk check is clean.\n[Used tools: shell]",
        "The disk check is clean.",
    ]);
    let remaining = Arc::clone(&provider.responses);
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("check the disk")];
    let tmp = TempDir::new().unwrap();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(tmp.path(), "check the disk").await;
    let (outcome, _trace) = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "terminal",
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
    .expect("the corrective retry should produce a deliverable answer");

    assert_eq!(
        remaining.lock().expect("responses lock should be valid").len(),
        0,
        "a fabricated marker with no tool execution must cost exactly one corrective retry \
                 (both scripted responses consumed = two provider calls)"
    );
    assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "The disk check is clean."));
    assert!(
        history
            .iter()
            .all(|message| !contains_fabricated_tool_marker(&message.content)),
        "no fabricated marker may reach history: {history:?}"
    );
}

#[tokio::test]
async fn repeated_fabricated_tool_marker_is_stripped_and_still_delivered() {
    let (result, history) = run_without_tools(vec![
        "Everything is green.\n[Used tools: shell, gateway]",
        "Everything is green.\n[Used tools: shell]",
    ])
    .await;
    let (outcome, _trace) = result.expect("a marker beside real text must not fail the turn");

    assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "Everything is green."));
    assert_eq!(history.len(), 3);
    assert_eq!(
        history.last().map(|message| message.content.as_str()),
        Some("Everything is green."),
        "the second fabrication must be stripped, not bailed on: {history:?}"
    );
}

#[tokio::test]
async fn marker_after_a_real_tool_batch_is_stripped_without_a_retry() {
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let provider = NativeScriptedProvider::new(vec![
        ChatResponse {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "file_read".to_string(),
                arguments: "{}".to_string(),
            }],
            reasoning_content: None,
        },
        ChatResponse {
            text: Some("The check ran.\n[Used tools: file_read]".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        },
    ]);
    let remaining = Arc::clone(&provider.responses);
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
        name: "file_read".to_string(),
        execution_order: Arc::clone(&execution_order),
    })];
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("run file_read to check the fixture"),
    ];
    let tmp = TempDir::new().unwrap();
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
        tmp.path(),
        "run file_read to check the fixture",
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
    .expect("a turn that really used a tool must complete");

    assert!(
        history.iter().any(|message| message.role == "tool"),
        "the turn must really have dispatched a tool batch: {history:?}"
    );
    assert_eq!(
        execution_order
            .lock()
            .expect("execution order lock should be valid")
            .as_slice(),
        ["file_read".to_string()],
        "the premise is a tool that really ran: a batch that was only dispatched and then \
                 refused is not tool use, and must not buy the model a free marker"
    );
    assert_eq!(
        remaining.lock().expect("responses lock should be valid").len(),
        0,
        "a marker backed by a real tool run must not spend a corrective retry"
    );
    assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "The check ran."));
}

/// A model can write the marker in the *same* response that carries real
/// tool calls. That text goes straight into history as the assistant
/// message requesting the batch, where the final-reply guard never sees
/// it — and from there into compaction, summaries and the next prompt.
///
/// MUTATION GUARD: drop the strip from `assistant_history_content` and
/// the history assertion fails.
#[tokio::test]
async fn a_marker_written_beside_real_tool_calls_never_reaches_history() {
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let provider = NativeScriptedProvider::new(vec![
        ChatResponse {
            text: Some("Checking now.\n[Used tools: file_read]".to_string()),
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "file_read".to_string(),
                arguments: "{}".to_string(),
            }],
            reasoning_content: None,
        },
        ChatResponse {
            text: Some("The check ran.".to_string()),
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
        ChatMessage::user("run file_read to check the fixture"),
    ];
    let tmp = TempDir::new().unwrap();
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
        tmp.path(),
        "run file_read to check the fixture",
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
    .expect("a turn that really used a tool must complete");

    assert!(
        history.iter().any(|message| message.role == "tool"),
        "the turn must really have dispatched a tool batch: {history:?}"
    );
    assert!(
        history.iter().all(|message| !message.content.contains("[Used tools:")),
        "the marker written alongside the tool calls must not be stored: {history:?}"
    );
    assert!(
        history.iter().any(|message| message.content.contains("Checking now.")),
        "only the marker is dropped; the model's own words stay: {history:?}"
    );
    assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "The check ran."));
}

/// Dispatching a batch is not using a tool. When every call in it came
/// back refused or failed, the turn did no work, so a `[Used tools: ...]`
/// line is still provably fabricated and still worth the one corrective
/// retry.
///
/// MUTATION GUARD: set `executed_any_tool_call` at dispatch time (the
/// shape this replaced) and the third scripted response is never
/// consumed, because no retry is spent.
#[tokio::test]
async fn a_batch_whose_calls_all_failed_still_buys_the_corrective_retry() {
    let provider = NativeScriptedProvider::new(vec![
        ChatResponse {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "file_read".to_string(),
                arguments: "{}".to_string(),
            }],
            reasoning_content: None,
        },
        ChatResponse {
            text: Some("The check is clean.\n[Used tools: file_read]".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        },
        ChatResponse {
            text: Some("The check is clean.".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        },
    ]);
    let remaining = Arc::clone(&provider.responses);
    let executions = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(RefusingTool {
        name: "file_read",
        executions: Arc::clone(&executions),
    })];
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("run file_read to check the fixture"),
    ];
    let tmp = TempDir::new().unwrap();
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
        tmp.path(),
        "run file_read to check the fixture",
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
                read_only_tool_concurrency_window: 3,
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
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .expect("the corrective retry should produce a deliverable answer");

    assert_eq!(
        remaining.lock().expect("responses lock should be valid").len(),
        0,
        "a batch that produced nothing but refusals must not count as tool use, \
                 so the fabricated marker still costs one corrective retry"
    );
    assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "The check is clean."));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

/// The gap the whole-line guard left: `Fixture checked. [Used tools:
/// shell]` is the same claim about work that never happened as a marker
/// on its own line, so it must cost the one corrective retry, and once
/// that retry is spent the marker must be cut out of the sentence
/// instead of delivered.
///
/// MUTATION GUARD: put the line-level `contains_fabricated_tool_marker`
/// back in the final-response guard and the second scripted response is
/// never consumed — the inline marker is delivered verbatim.
#[tokio::test]
async fn an_inline_fabricated_marker_without_tool_execution_retries_once_then_is_stripped() {
    let provider = ScriptedProvider::from_text_responses(vec![
        "Fixture checked. [Used tools: shell]",
        "Fixture checked. [Used tools: shell] Nothing else to report.",
    ]);
    let remaining = Arc::clone(&provider.responses);
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("check the fixture")];
    let tmp = TempDir::new().unwrap();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(tmp.path(), "check the fixture").await;
    let (outcome, _trace) = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "terminal",
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
    .expect("a marker beside real text must not fail the turn");

    assert_eq!(
        remaining.lock().expect("responses lock should be valid").len(),
        0,
        "an in-line fabricated marker with no tool execution must cost exactly one \
                 corrective retry (both scripted responses consumed = two provider calls)"
    );
    assert!(
        matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "Fixture checked. Nothing else to report."),
        "the retry is spent, so only the marker is cut out of the sentence: {outcome:?}"
    );
    assert!(
        history.iter().all(|message| !message.content.contains("[Used tools:")),
        "no fabricated marker may reach history in either grain: {history:?}"
    );
}

/// Streaming delivery must see exactly what the returned text says. The
/// loop chunks `display_text` into the `on_delta` sender *after* the
/// marker guard, so the stream may not paint a marker the final answer
/// no longer contains.
///
/// MUTATION GUARD: move the strip below the streaming block and the
/// reassembled stream carries `[Used tools:` while the outcome does not.
#[tokio::test]
async fn an_inline_marker_is_stripped_before_it_can_be_streamed() {
    let provider = ScriptedProvider::from_text_responses(vec![
        "Fixture checked. [Used tools: shell]",
        "Fixture checked. [Used tools: shell] Nothing else to report.",
    ]);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("check the fixture")];
    let tmp = TempDir::new().unwrap();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(tmp.path(), "check the fixture").await;
    let (outcome, _trace) = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "terminal",
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
            hooks: &crate::hooks::HookManager::new(tmp.path().to_path_buf()),
            approval: None,
            approval_resolver: None,
            runtime_adapter: Some(admission.adapter()),
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: Some(tx),
            on_tool_call: None,
        },
    )
    .await
    .expect("a marker beside real text must not fail the turn");

    let mut streamed = String::new();
    while let Ok(chunk) = rx.try_recv() {
        streamed.push_str(&chunk);
    }
    assert!(
        !streamed.contains("[Used tools:"),
        "the streamed draft must never paint a fabricated marker: {streamed:?}"
    );
    assert!(
        streamed.contains("Fixture checked."),
        "the model's own words must still stream: {streamed:?}"
    );
    assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "Fixture checked. Nothing else to report."));
}

/// A batch whose every call was refused did no work, so an in-line
/// marker after it is still provably fabricated and still worth the one
/// corrective retry — same rule the whole-line grain already enforced.
///
/// MUTATION GUARD: restore the line-level detector and the third
/// scripted response is never consumed, because no retry is spent.
#[tokio::test]
async fn a_denied_only_batch_followed_by_an_inline_marker_still_retries() {
    let provider = NativeScriptedProvider::new(vec![
        ChatResponse {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "refusing_tool".to_string(),
                arguments: "{}".to_string(),
            }],
            reasoning_content: None,
        },
        ChatResponse {
            text: Some("Fixture checked. [Used tools: refusing_tool]".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        },
        ChatResponse {
            text: Some("Fixture checked; the tool refused.".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        },
    ]);
    let remaining = Arc::clone(&provider.responses);
    let executions = Arc::new(AtomicUsize::new(0));
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(RefusingTool {
        name: "refusing_tool",
        executions: Arc::clone(&executions),
    })];
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("run the check")];
    let tmp = TempDir::new().unwrap();
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let mut config = crate::config::Config::default();
    config.workspace_dir = tmp.path().to_path_buf();
    config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_config(
        config,
        "run the check",
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
                read_only_tool_concurrency_window: 3,
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
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .expect("the corrective retry should produce a deliverable answer");

    assert_eq!(
        remaining.lock().expect("responses lock should be valid").len(),
        0,
        "a batch that produced nothing but refusals must not count as tool use, so the \
                 in-line fabricated marker still costs one corrective retry"
    );
    assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "Fixture checked; the tool refused."));
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

/// Tools really ran, so the marker buys no retry — but it is still a
/// fabricated shape that must not be stored where the next turn would
/// read it, in either grain.
///
/// MUTATION GUARD: restore the line-level strip on
/// `assistant_history_content` and the history assertion fails.
#[tokio::test]
async fn an_inline_marker_beside_real_tool_calls_is_stripped_without_a_retry() {
    let execution_order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let provider = NativeScriptedProvider::new(vec![
        ChatResponse {
            text: Some("Checking now. [Used tools: file_read] One moment.".to_string()),
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "file_read".to_string(),
                arguments: "{}".to_string(),
            }],
            reasoning_content: None,
        },
        ChatResponse {
            text: Some("Fixture checked. [Used tools: file_read]".to_string()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        },
    ]);
    let remaining = Arc::clone(&provider.responses);
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(RecordingTool {
        name: "file_read".to_string(),
        execution_order: Arc::clone(&execution_order),
    })];
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("run file_read to check the fixture"),
    ];
    let tmp = TempDir::new().unwrap();
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
        tmp.path(),
        "run file_read to check the fixture",
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
    .expect("a turn that really used a tool must complete");

    assert_eq!(
        execution_order
            .lock()
            .expect("execution order lock should be valid")
            .as_slice(),
        ["file_read".to_string()],
        "the premise is a tool that really ran"
    );
    assert_eq!(
        remaining.lock().expect("responses lock should be valid").len(),
        0,
        "a marker backed by a real tool run must not spend a corrective retry"
    );
    assert!(
        history.iter().all(|message| !message.content.contains("[Used tools:")),
        "neither grain of the marker may be stored: {history:?}"
    );
    assert!(
        history
            .iter()
            .any(|message| message.content.contains("Checking now. One moment.")),
        "only the marker is cut out; the model's own words stay: {history:?}"
    );
    assert!(matches!(outcome, ToolLoopOutcome::Text(ref text) if text == "Fixture checked."));
}

/// The summarizer reads the turns it is compacting and is explicitly
/// asked to preserve recent exchanges verbatim, so a marker in those
/// turns comes back inside the summary — which is then replayed on every
/// later turn and persisted as compaction audit.
///
/// MUTATION GUARD: drop the strip applied to `summary` in
/// `build_configurable_compaction_patch_with_source_history` and the
/// replacement message carries `[Used tools:`.
#[tokio::test]
async fn a_compaction_summary_carrying_a_marker_is_stripped_in_both_grains() {
    let provider = SystemSummaryProvider {
        response: "## Decisions\n- keep the fixture\n## Open TODOs\n- none\n\
                           ## Critical Context\n- Fixture checked. [Used tools: shell]\n\
                           ## Progress\n[Used tools: shell, file_read]"
            .to_string(),
    };
    let mut history = vec![ChatMessage::system("sys")];
    for idx in 0..10 {
        history.push(ChatMessage::user(format!("turn-{idx} {}", "bulk-token ".repeat(80))));
    }
    let config = crate::config::AgentCompactionConfig {
        mode: crate::config::AgentCompactionMode::Safeguard,
        reserve_tokens: 10,
        keep_recent_messages: 2,
        memory_flush: false,
        max_context_tokens: 120,
        max_context_tokens_explicit: true,
        os_paging: crate::config::OsPagingConfig::default(),
    };

    let patch = build_configurable_compaction_patch(
        &history,
        &provider,
        "model",
        &config,
        None,
        "test_inline_marker_compaction",
    )
    .await
    .expect("compaction must not fail")
    .expect("an over-budget history must produce a compaction patch");

    assert!(
        patch
            .replacement
            .iter()
            .all(|message| !message.content.contains("[Used tools:")),
        "a compaction summary is replayed on every later turn: {:?}",
        patch.replacement
    );
    assert!(
        patch
            .replacement
            .iter()
            .any(|message| message.content.contains("Fixture checked.")),
        "only the marker is removed; the summary itself survives: {:?}",
        patch.replacement
    );
}

/// Case 1 of the 2026-09-19 cross-audit fence probe, replayed verbatim
/// through the delivery path: a `~~~text` block whose body holds the
/// line ```` ```example ````, closed by `~~~`, and then a prose marker.
/// The toggle model counted that body line as the closer and the real
/// closer as a new opener, so the prose was mistaken for code: no
/// corrective retry was spent and the fabricated claim shipped to the
/// user and into history.
///
/// MUTATION GUARD: accept any fence line as a closer again (drop the
/// character/length check in `CodeFence::closes`) and the second
/// scripted response is never consumed, the outcome carries
/// `[Used tools: shell]`, and history stores it for the next turn to
/// imitate.
#[tokio::test]
async fn a_marker_after_a_mixed_fence_block_retries_once_then_is_stripped() {
    let provider = ScriptedProvider::from_text_responses(vec![
        "~~~text\n```example\n~~~\nFixture checked. [Used tools: shell]",
        "~~~text\n```example\n~~~\nFixture checked. [Used tools: shell] Nothing else to report.",
    ]);
    let remaining = Arc::clone(&provider.responses);
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("check the fixture")];
    let tmp = TempDir::new().unwrap();
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::empty(tmp.path(), "check the fixture").await;
    let (outcome, _trace) = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "terminal",
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
    .expect("a marker beside real text must not fail the turn");

    assert_eq!(
        remaining.lock().expect("responses lock should be valid").len(),
        0,
        "prose after a mixed-fence block is not code, so the fabricated marker must \
                 cost exactly one corrective retry (both scripted responses consumed)"
    );
    assert!(
        matches!(
            outcome,
            ToolLoopOutcome::Text(ref text)
                if text == "~~~text\n```example\n~~~\nFixture checked. Nothing else to report."
        ),
        "the fenced example must survive byte-for-byte and only the prose marker is cut: {outcome:?}"
    );
    assert!(
        history.iter().all(|message| !message.content.contains("[Used tools:")),
        "no fabricated marker may reach history: {history:?}"
    );
    assert!(
        history.iter().any(|message| message.content.contains("```example")),
        "the model's own fenced example must still be stored: {history:?}"
    );
}

/// Case 2 of the same probe, the mirror image: a ```` ````text ````
/// block whose body holds a bare ```` ``` ```` line is code all the way
/// to its four-backtick closer. The marker inside is the model's own
/// example of the syntax, so it must neither buy the corrective retry
/// nor be edited out of the answer or out of history.
///
/// MUTATION GUARD: accept any fence line as a closer again and the bare
/// ```` ``` ```` line ends the block, so the loop spends the retry on a
/// genuine code example and then delivers the example with a hole in it.
#[tokio::test]
async fn a_marker_inside_a_long_fence_block_is_neither_retried_nor_edited() {
    const ANSWER: &str = "````text\n```\nDocumented [Used tools: shell]\n````";
    let provider = ScriptedProvider::from_text_responses(vec![
        ANSWER,
        "a genuine code example must not spend the corrective retry",
    ]);
    let remaining = Arc::clone(&provider.responses);
    let mut history = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("show me the marker syntax"),
    ];
    let tmp = TempDir::new().unwrap();
    let admission =
        crate::agent::turn_tests::admission::AdmittedTurn::empty(tmp.path(), "show me the marker syntax").await;
    let (outcome, _trace) = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "terminal",
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
    .expect("a fenced code example must not fail the turn");

    assert_eq!(
        remaining.lock().expect("responses lock should be valid").len(),
        1,
        "a marker inside a genuine fenced block is code, not a claim about work: \
                 the second scripted response must stay unconsumed"
    );
    assert!(
        matches!(outcome, ToolLoopOutcome::Text(ref text) if text == ANSWER),
        "a fenced code example must be delivered byte-for-byte: {outcome:?}"
    );
    assert_eq!(
        history.last().map(|message| message.content.as_str()),
        Some(ANSWER),
        "the history copy of a fenced code example must not be edited either: {history:?}"
    );
}

async fn run_with_response(response: &str, expose: bool) -> (ToolLoopOutcome, Vec<ChatMessage>) {
    let provider = ScriptedProvider::from_text_responses(vec![response]);
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(StaySilentTool::new())];
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("some group chatter")];
    let tmp = TempDir::new().unwrap();
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission = crate::agent::turn_tests::admission::AdmittedTurn::with_tools(
        tmp.path(),
        "some group chatter",
        Arc::clone(&tools_registry),
    )
    .await;
    let (outcome, _trace) = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "telegram",
            scope: None,
            mode: ChatMode::default(),
            silent: true,
            expose_stay_silent: expose,
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
            memory: ToolLoopMemory::none(),
        },
        crate::agent::turn_request::TurnDelivery {
            cancellation: None,
            on_delta: None,
            on_tool_call: None,
        },
    )
    .await
    .expect("loop should complete");
    (outcome, history)
}

#[tokio::test]
async fn stay_silent_call_produces_silent_outcome() {
    let resp = "<tool_call>\n{\"name\":\"stay_silent\",\"arguments\":{\"reason\":\"off-topic banter\"}}\n</tool_call>";
    let (outcome, _history) = run_with_response(resp, true).await;
    match outcome {
        ToolLoopOutcome::Silent { reason } => assert_eq!(reason, "off-topic banter"),
        other => panic!("expected Silent, got {other:?}"),
    }
}

#[tokio::test]
async fn silent_outcome_writes_no_assistant_turn() {
    let resp = "<tool_call>\n{\"name\":\"stay_silent\",\"arguments\":{\"reason\":\"not relevant\"}}\n</tool_call>";
    let before = 2; // system + user
    let (outcome, history) = run_with_response(resp, true).await;
    assert!(matches!(outcome, ToolLoopOutcome::Silent { .. }));
    // No assistant / tool turn appended: silent leaves zero history trace.
    assert_eq!(
        history.len(),
        before,
        "silent turn must not append assistant/tool history, got: {history:?}"
    );
    assert!(
        history.iter().all(|m| m.role != "assistant"),
        "no assistant turn should be written on a silent decision"
    );
}

#[tokio::test]
async fn stay_silent_not_honored_when_not_exposed() {
    // When the tool is NOT exposed (DM / non-smart), a stay_silent call
    // is rejected outside the advertised capability set and the loop
    // continues — it never short-circuits to Silent. The model
    // then has no further scripted response, so the loop errors out
    // (exhausted responses), proving Silent was NOT taken.
    let resp = "<tool_call>\n{\"name\":\"stay_silent\",\"arguments\":{\"reason\":\"x\"}}\n</tool_call>";
    let provider = ScriptedProvider::from_text_responses(vec![resp]);
    let tools_registry: Vec<Box<dyn Tool>> = vec![Box::new(StaySilentTool::new())];
    let mut history = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
    let tmp = TempDir::new().unwrap();
    let tools_registry = crate::agent::turn_tests::admission::registry_with_search(tools_registry);
    let admission =
        crate::agent::turn_tests::admission::AdmittedTurn::with_tools(tmp.path(), "hi", Arc::clone(&tools_registry))
            .await;
    let result = run_turn(
        &mut history,
        crate::agent::turn_request::TurnRequest {
            channel_name: "telegram",
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
            // not exposed
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
    // The single scripted response was consumed continuing the loop, so a
    // second LLM call is attempted and fails (exhausted) — i.e. NOT Silent.
    assert!(
        result.is_err(),
        "non-exposed stay_silent must continue the loop after rejection, not short-circuit to Silent"
    );
    let error = result.expect_err("non-exposed silence must continue to the next provider call");
    assert!(
        error
            .chain()
            .any(|cause| cause.to_string().contains("scripted provider exhausted responses")),
        "unexpected early failure: {error:#}"
    );
}
