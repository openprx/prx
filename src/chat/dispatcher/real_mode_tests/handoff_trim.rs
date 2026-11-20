#![cfg(test)]
//! The redux driver's pre-turn trim keeps the newest validated context
//! handoff while older turns can still go (REQ-HANDOFF-RECALL-AND-TRIM M2).

use super::*;
use crate::providers::traits::{
    ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
};
use futures::stream::{self, BoxStream, StreamExt};

const HANDOFF_ID: &str = "handoff-redux";
const OLDEST_TURN: &str = "redux-oldest-turn ";

/// Records the first streamed request and answers it.
struct FirstRequestProvider {
    first_request: Arc<Mutex<Option<Vec<PMsg>>>>,
}

#[async_trait::async_trait]
impl Provider for FirstRequestProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        anyhow::bail!("test: no summarizer runs with compaction off")
    }
    async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        anyhow::bail!("test: the driver streams")
    }
    fn supports_streaming(&self) -> bool {
        true
    }
    fn stream_chat_with_history(
        &self,
        messages: &[PMsg],
        _: &str,
        _: f64,
        _: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        self.first_request.lock().get_or_insert_with(|| messages.to_vec());
        stream::iter(vec![
            Ok(StreamChunk::delta("redux-handoff-ok")),
            Ok(StreamChunk::final_chunk()),
        ])
        .boxed()
    }
    async fn warmup(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// One turn through the real `StartTurn` effect over an over-budget view
/// whose handoff is `shown` (the stored note, or a chat view's compact
/// rendering with `guard` as its canonical history). Returns the first
/// request the provider saw.
async fn first_request_of_trimmed_turn(history: Vec<PMsg>, guard: Option<Vec<PMsg>>) -> Vec<PMsg> {
    let first_request = Arc::new(Mutex::new(None));
    let memory: Arc<dyn Memory> = Arc::new(NoneMemory::new());
    let (mut deps, mut action_rx, _hooks, _temp) = build_deps(memory, CancellationToken::new());
    deps.provider = Arc::new(FirstRequestProvider {
        first_request: Arc::clone(&first_request),
    });
    let fixture_deps = deps.clone();
    let fixture_cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
        .expect("test: fixture cleanup owner");
    fixture_cleanup
        .run(async {
            let executor = EffectExecutor::new_with_deps(deps);
            let config = crate::config::AgentCompactionConfig {
                mode: crate::config::AgentCompactionMode::Off,
                reserve_tokens: 10,
                keep_recent_messages: 2,
                memory_flush: false,
                max_context_tokens: 6_000,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };
            assert!(
                crate::agent::context_budget::plan_context_budget(
                    &history,
                    &config,
                    crate::agent::context_budget::PRE_TURN_FLUSH_THRESHOLD
                )
                .over_hard_limit,
                "premise: the turn starts over its window"
            );
            let binding = admit_start_turn_fixture(&fixture_deps, "draft-handoff-redux", "current request", false)
                .await
                .expect("test: genuine admission prerequisite");
            executor
                .execute(Effect::StartTurn {
                    provider_turn_task_id: None,
                    draft_id: "draft-handoff-redux".into(),
                    history,
                    compaction_guard_history: guard
                        .map(crate::agent::turn_history::CanonicalHistory::from_test_messages),
                    compaction_config: Some(config),
                    cancel: CancellationToken::new(),
                    chat_mode: crate::agent::turn_contracts::ChatMode::Edit,
                    turn_spawn_ctx: None,
                    turn_message_send_ctx: None,
                    routing_input: None,
                    turn_binding: Some(binding),
                })
                .await;

            let mut completed = false;
            for _ in 0..16 {
                let action = tokio::time::timeout(Duration::from_secs(10), action_rx.recv())
                    .await
                    .expect("test: driver action within its deadline")
                    .expect("test: the action channel stays open");
                match action {
                    Action::StreamCompleted { final_text, .. } => {
                        assert!(final_text.contains("redux-handoff-ok"));
                        completed = true;
                        break;
                    }
                    Action::StreamFailed { err, .. } => {
                        panic!("the trimmed turn must reach the provider, not fail: {err}");
                    }
                    _ => {}
                }
            }
            assert!(completed, "the driver completes the trimmed turn");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .expect("test: fixture cleanup");
    first_request
        .lock()
        .clone()
        .expect("test: the provider saw the first request")
}

/// `[sys, handoff, 24 long turns, request]`.
fn over_budget_view(handoff: PMsg) -> Vec<PMsg> {
    let mut history = vec![PMsg::system("sys"), handoff];
    for turn in 0..24 {
        let marker = if turn == 0 { OLDEST_TURN } else { "turn " };
        history.push(PMsg::user(format!("{marker}{turn} {}", "long context ".repeat(200))));
    }
    history.push(PMsg::user("current request"));
    history
}

/// The redux preflight's trim fallback (compaction off, so no rollover can
/// be built), for a view holding the stored note and for a chat view holding
/// its compact rendering: the first provider request has lost the oldest
/// turns but still shows the handoff, byte for byte, behind the system prompt.
#[tokio::test]
async fn newest_handoff_is_trimmed_last_on_redux_fallback() {
    let stored = PMsg::assistant(crate::agent::compaction::handoff_message_for_test(
        HANDOFF_ID,
        &["row-a", "row-b", "row-c"],
        1,
    ));
    let rendered = PMsg::assistant(
        crate::agent::compaction::render_context_handoff_for_provider(&stored.role, &stored.content).into_owned(),
    );
    assert!(
        rendered.content != stored.content
            && crate::agent::compaction::is_validated_context_handoff_message(&stored)
            && crate::agent::compaction::is_validated_context_handoff_message(&rendered),
        "premise: both fixture forms are validated handoffs"
    );
    let cases = [
        ("stored note", stored.clone(), None),
        ("compact rendering", rendered, Some(over_budget_view(stored))),
    ];
    for (form, shown, guard) in cases {
        let sent = first_request_of_trimmed_turn(over_budget_view(shown.clone()), guard).await;
        assert!(
            !sent.iter().any(|message| message.content.starts_with(OLDEST_TURN)),
            "{form}, premise: the redux preflight trimmed the oldest turn"
        );
        assert!(
            sent.iter()
                .any(|message| message.role == shown.role && message.content == shown.content),
            "{form}: the redux trim keeps the handoff"
        );
        assert!(
            sent.iter().any(|message| message.content.contains("current request")),
            "{form}: the request reaches the provider"
        );
    }
}
