//! R4: a provider-turn worker's failure is terminal once, and its exit
//! cleanup always runs.
//!
//! Real dispatcher task (reducer + real executor), real `TurnScheduler`,
//! `ProviderTurnWorkerRegistry` and `TurnCompletionSignal`; the test plays the
//! chat run loop's part by recording lifecycle events through the production
//! `record_provider_turn_lifecycle_event` and routing completions through
//! `route_provider_completion_event`. Every wait is bounded.

#![cfg(test)]

use super::*;
use crate::chat::provider_turns::{
    OrderedCommitTerminal, ProviderTurnCompletionEvent, dispatch_ordered_provider_turn_commit_and_wait,
    record_provider_turn_lifecycle_event, route_provider_completion_event,
};
use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskId};
use crate::chat::turn_supervisor::{
    WORKER_FAILED_ERROR, WorkerLease, WorkerSupervision, supervise_provider_turn_worker,
};
use crate::chat::turn_worker::{ProviderTurnWorkerKind, ProviderTurnWorkerRegistry};
use crate::llm::route_decision::TokenUsageSource;
use crate::providers::traits::{
    ChatMessage as PMsg, ChatRequest, ChatResponse, ProviderCapabilities, StreamChunk, StreamOptions, StreamResult,
};
use futures::stream::{self, BoxStream, StreamExt};
use std::collections::{HashMap, VecDeque};

/// Upper bound for any single wait in these tests.
const STEP_DEADLINE: Duration = Duration::from_secs(30);
/// Lifecycle channel capacity (three events per turn, a handful of turns).
const LIFECYCLE_CAPACITY: usize = 64;
const REQUEST: &str = "supervised request";
const ANSWER: &str = "supervised answer";

#[derive(Clone, Copy, Debug)]
enum StreamScript {
    Panic,
    Answer,
    Hang,
}

/// A streaming provider that follows a per-call script.
struct ScriptedStreamProvider {
    script: Mutex<VecDeque<StreamScript>>,
}

impl ScriptedStreamProvider {
    fn new(script: &[StreamScript]) -> Self {
        Self {
            script: Mutex::new(script.iter().copied().collect()),
        }
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedStreamProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }
    async fn chat_with_system(&self, _: Option<&str>, _: &str, _: &str, _: f64) -> anyhow::Result<String> {
        Ok(String::new())
    }
    async fn chat(&self, _: ChatRequest<'_>, _: &str, _: f64) -> anyhow::Result<ChatResponse> {
        Ok(ChatResponse {
            text: Some(ANSWER.to_owned()),
            tool_calls: Vec::new(),
            reasoning_content: None,
        })
    }
    fn supports_streaming(&self) -> bool {
        true
    }
    fn stream_chat_with_history(
        &self,
        _: &[PMsg],
        _: &str,
        _: f64,
        _: StreamOptions,
    ) -> BoxStream<'static, StreamResult<StreamChunk>> {
        let step = self.script.lock().pop_front().unwrap_or(StreamScript::Answer);
        match step {
            StreamScript::Panic => stream::poll_fn(|_| -> std::task::Poll<Option<StreamResult<StreamChunk>>> {
                panic!("scripted provider stream panic")
            })
            .boxed(),
            StreamScript::Answer => {
                stream::iter(vec![Ok(StreamChunk::delta(ANSWER)), Ok(StreamChunk::final_chunk())]).boxed()
            }
            StreamScript::Hang => stream::pending().boxed(),
        }
    }
    async fn warmup(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// The run loop's share of provider-turn bookkeeping.
struct RunLoop {
    scheduler: TurnScheduler,
    workers: ProviderTurnWorkerRegistry,
    lifecycle_rx: mpsc::Receiver<ProviderTurnLifecycleEvent>,
    lifecycle_open: bool,
    pending: HashMap<TurnTaskId, ProviderTurnCompletionEvent>,
    signal: TurnCompletionSignal,
}

impl RunLoop {
    fn new(lifecycle_rx: mpsc::Receiver<ProviderTurnLifecycleEvent>, signal: TurnCompletionSignal) -> Self {
        Self {
            scheduler: TurnScheduler::new(),
            workers: ProviderTurnWorkerRegistry::new(),
            lifecycle_rx,
            lifecycle_open: true,
            pending: HashMap::new(),
            signal,
        }
    }

    /// Schedule and register a provider task; returns it with its
    /// completion waiter armed before anything is dispatched.
    fn schedule(&mut self, draft_id: &str) -> (TurnTaskId, tokio::sync::futures::OwnedNotified) {
        let task = self.scheduler.enqueue(draft_id, TurnPriority::Normal, 0);
        self.scheduler.start_task(task).expect("scheduled task starts");
        let scheduled = self.scheduler.task(task).expect("scheduled task exists");
        self.workers
            .start_from_task(scheduled, ProviderTurnWorkerKind::Detached)
            .expect("worker registered");
        self.signal.register_turn(task, draft_id);
        let notified = self.signal.notified_for(task).expect("task-scoped waiter");
        (task, notified)
    }

    /// Record lifecycle events until `notified` fires; then route the
    /// completion as the run loop does and return the turn's outcome.
    async fn await_outcome(
        &mut self,
        task: TurnTaskId,
        notified: tokio::sync::futures::OwnedNotified,
    ) -> Option<TurnOutcomeKind> {
        let mut notified = std::pin::pin!(notified);
        tokio::time::timeout(STEP_DEADLINE, async {
            loop {
                tokio::select! {
                    () = &mut notified => break,
                    event = self.lifecycle_rx.recv() => {
                        let event = event.expect("lifecycle channel open");
                        record_provider_turn_lifecycle_event(&mut self.workers, event);
                    }
                }
            }
        })
        .await
        .expect("turn outcome within the deadline");
        let outcome = self.signal.consume_turn_outcome(task);
        let usage = self.signal.consume_turn_usage(task);
        self.signal.unregister_turn(task);
        let completion = ProviderTurnCompletionEvent {
            task_id: task,
            outcome: outcome.clone(),
            usage,
        };
        let routed = route_provider_completion_event(
            &mut self.lifecycle_rx,
            &mut self.lifecycle_open,
            &mut self.workers,
            &mut self.pending,
            Some(task),
            completion,
        );
        assert!(routed.is_some(), "the current task's completion is routed");
        outcome
    }

    /// Record lifecycle events until the worker's lease is retired.
    async fn await_exit(&mut self, task: TurnTaskId) {
        tokio::time::timeout(STEP_DEADLINE, async {
            while !self.workers.worker(task).is_some_and(|worker| worker.execution_exited) {
                let event = self.lifecycle_rx.recv().await.expect("lifecycle channel open");
                record_provider_turn_lifecycle_event(&mut self.workers, event);
            }
        })
        .await
        .expect("worker exit within the deadline");
    }

    fn assert_retired(&self, task: TurnTaskId) {
        let worker = self.workers.worker(task).expect("worker known");
        assert!(worker.execution_exited, "exit recorded");
        assert!(!worker.execution_lease_active, "lease retired");
        assert!(!self.workers.execution_handle_attached(task), "abort handle retired");
    }
}

fn start_llm_turn(
    task: TurnTaskId,
    draft_id: &str,
    cancel: CancellationToken,
    turn_binding: Option<Arc<crate::chat::binding::ChatTurnBinding>>,
) -> Action {
    Action::StartLLMTurn {
        provider_turn_task_id: Some(task),
        provider_turn_sequence: Some(task.get()),
        draft_id: draft_id.to_owned(),
        history: vec![PMsg::user(REQUEST)],
        compaction_guard_history: None,
        compaction_config: None,
        cancel,
        turn_spawn_ctx: None,
        turn_message_send_ctx: None,
        routing_input: None,
        turn_binding,
    }
}

#[tokio::test]
async fn a_panicking_stream_fails_its_turn_once_and_the_chat_keeps_working() {
    Box::pin(async {
        let shutdown = CancellationToken::new();
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let (mut deps, _unused_actions, _hooks, _temp) = build_deps(Arc::new(NoneMemory::new()), shutdown.clone());
        deps.persistence.action_tx = dispatcher.sender();
        deps.provider = Arc::new(ScriptedStreamProvider::new(&[
            StreamScript::Panic,
            StreamScript::Answer,
            StreamScript::Hang,
            StreamScript::Hang,
        ]));
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel(LIFECYCLE_CAPACITY);
        deps.provider_turn_lifecycle_tx = Some(lifecycle_tx);
        let actions = deps.persistence.action_tx.clone();
        let fixture_deps = deps.clone();
        let signal = TurnCompletionSignal::new();
        let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, CancellationToken::new())
            .expect("fixture cleanup owner");
        cleanup
            .run(async {
                let state = ChatState::new(Arc::from("ollama"), Arc::from("test-model"), shutdown.clone());
                let dispatcher_task = spawn_dispatcher_task_with_signal(
                    state,
                    action_rx,
                    shutdown.clone(),
                    EffectExecutor::new_with_deps(deps).with_session_publisher(dispatcher.session_publisher()),
                    Some(signal.clone()),
                );
                let mut run_loop = RunLoop::new(lifecycle_rx, signal.clone());

                // (a) The stream panics: one accepted StreamFailed, signal Failed, lease retired.
                let (panicked, notified) = run_loop.schedule("draft-panic");
                let binding = admit_start_turn_fixture(&fixture_deps, "draft-panic", REQUEST, false).await?;
                actions
                    .send(start_llm_turn(
                        panicked,
                        "draft-panic",
                        CancellationToken::new(),
                        Some(binding),
                    ))
                    .await?;
                let outcome = run_loop.await_outcome(panicked, notified).await;
                assert!(
                    matches!(
                        &outcome,
                        Some(TurnOutcomeKind::Failed { err, retryable: false }) if err == WORKER_FAILED_ERROR
                    ),
                    "the panic fails the turn: {outcome:?}"
                );
                run_loop.await_exit(panicked).await;
                run_loop.assert_retired(panicked);
                assert!(
                    run_loop
                        .workers
                        .worker(panicked)
                        .is_some_and(|worker| worker.execution_failed)
                );
                assert!(
                    signal.consume_turn_outcome(panicked).is_none(),
                    "exactly one terminal outcome"
                );

                // The next StartLLMTurn completes.
                let (next, notified) = run_loop.schedule("draft-next");
                let binding = admit_start_turn_fixture(&fixture_deps, "draft-next", REQUEST, false).await?;
                actions
                    .send(start_llm_turn(
                        next,
                        "draft-next",
                        CancellationToken::new(),
                        Some(binding),
                    ))
                    .await?;
                match run_loop.await_outcome(next, notified).await {
                    Some(TurnOutcomeKind::Completed { final_text, .. }) => assert_eq!(final_text, ANSWER),
                    other => panic!("the next turn must complete, got {other:?}"),
                }
                // Phase 2: the ordered commit retires the ready draft.
                dispatch_ordered_provider_turn_commit_and_wait(
                    &dispatcher,
                    &signal,
                    next,
                    OrderedCommitTerminal::StreamCompleted {
                        draft_id: "draft-next",
                        reasoning: "",
                    },
                    REQUEST,
                    ANSWER,
                    false,
                    true,
                )
                .await
                .map_err(anyhow::Error::msg)?;
                run_loop.await_exit(next).await;
                run_loop.assert_retired(next);

                // CancelRequested takes effect on a running turn.
                let (cancelled, notified) = run_loop.schedule("draft-cancel");
                let binding = admit_start_turn_fixture(&fixture_deps, "draft-cancel", REQUEST, false).await?;
                actions
                    .send(start_llm_turn(
                        cancelled,
                        "draft-cancel",
                        CancellationToken::new(),
                        Some(binding),
                    ))
                    .await?;
                actions.send(Action::CancelRequested).await?;
                let outcome = run_loop.await_outcome(cancelled, notified).await;
                assert!(
                    matches!(outcome, Some(TurnOutcomeKind::Cancelled)),
                    "cancelled: {outcome:?}"
                );
                run_loop.await_exit(cancelled).await;
                run_loop.assert_retired(cancelled);

                // ShutdownRequested takes effect: the running turn is cancelled,
                // the shutdown token fires and the dispatcher task ends.
                let (stopped, notified) = run_loop.schedule("draft-shutdown");
                let binding = admit_start_turn_fixture(&fixture_deps, "draft-shutdown", REQUEST, false).await?;
                actions
                    .send(start_llm_turn(
                        stopped,
                        "draft-shutdown",
                        CancellationToken::new(),
                        Some(binding),
                    ))
                    .await?;
                actions.send(Action::ShutdownRequested).await?;
                let outcome = run_loop.await_outcome(stopped, notified).await;
                assert!(
                    matches!(outcome, None | Some(TurnOutcomeKind::Cancelled)),
                    "shutdown ends the turn without a failure: {outcome:?}"
                );
                assert!(shutdown.is_cancelled(), "ShutdownRequested fired the shutdown token");
                tokio::time::timeout(STEP_DEADLINE, dispatcher_task)
                    .await
                    .expect("dispatcher task ends after shutdown")?;
                run_loop.await_exit(stopped).await;
                run_loop.assert_retired(stopped);

                // With the dispatcher gone its input channel is closed: a
                // late action is refused instead of waiting forever.
                assert!(
                    actions.send(Action::RedrawRequested).await.is_err(),
                    "the closed dispatcher input refuses actions"
                );
                Ok::<_, anyhow::Error>(())
            })
            .await
            .expect("fixture cleanup");
    })
    .await;
}

/// A dispatcher with one visible task-scoped draft and a supervised test
/// worker driven step by step.
struct SupervisedTurn {
    dispatcher: ChatDispatcher,
    run_loop: RunLoop,
    lifecycle_tx: mpsc::Sender<ProviderTurnLifecycleEvent>,
    task: TurnTaskId,
    notified: Option<tokio::sync::futures::OwnedNotified>,
    _hooks: Arc<crate::hooks::HookManager>,
    _temp: TempDir,
}

const DRAFT: &str = "draft-supervised";
const LEASE: u64 = 9_001;

impl SupervisedTurn {
    fn new() -> Self {
        let (dispatcher, action_rx) = ChatDispatcher::new();
        let shutdown = CancellationToken::new();
        let (mut deps, _unused_actions, hooks, temp) = build_deps(Arc::new(NoneMemory::new()), shutdown.clone());
        deps.persistence.action_tx = dispatcher.sender();
        let executor = EffectExecutor::new_with_deps(deps).with_session_publisher(dispatcher.session_publisher());
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel(LIFECYCLE_CAPACITY);
        let signal = TurnCompletionSignal::new();
        let mut run_loop = RunLoop::new(lifecycle_rx, signal.clone());
        let (task, notified) = run_loop.schedule(DRAFT);
        let mut state = ChatState::new(Arc::from("ollama"), Arc::from("test-model"), shutdown.clone());
        // The reducer shows the draft; the worker below stands in for the
        // StartTurn effect, so the effects are not executed.
        let _ = state.reduce(start_llm_turn(task, DRAFT, CancellationToken::new(), None));
        let _dispatcher_task = spawn_dispatcher_task_with_signal(state, action_rx, shutdown, executor, Some(signal));
        Self {
            dispatcher,
            run_loop,
            lifecycle_tx,
            task,
            notified: Some(notified),
            _hooks: hooks,
            _temp: temp,
        }
    }

    /// Spawn `worker` under the production supervisor, attached as the
    /// executor attaches a real worker.
    async fn supervise(&self, worker: impl std::future::Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(worker);
        self.lifecycle_tx
            .send(ProviderTurnLifecycleEvent::HandleAttached {
                task_id: self.task,
                lease_id: LEASE,
                abort_handle: handle.abort_handle(),
            })
            .await
            .expect("lifecycle channel open");
        tokio::spawn(supervise_provider_turn_worker(
            handle,
            WorkerSupervision {
                lease: Some(WorkerLease {
                    task_id: self.task,
                    lease_id: LEASE,
                    lifecycle_tx: self.lifecycle_tx.clone(),
                }),
                draft_id: DRAFT.to_owned(),
                action_tx: self.dispatcher.sender(),
            },
        ));
    }

    /// A worker that starts, reports phase 1 with nonzero usage, then
    /// panics when `release` fires.
    fn ready_then_panic_worker(
        &self,
        release: tokio::sync::oneshot::Receiver<()>,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let lifecycle_tx = self.lifecycle_tx.clone();
        let actions = self.dispatcher.sender();
        let task_id = self.task;
        async move {
            lifecycle_tx
                .send(ProviderTurnLifecycleEvent::Started {
                    task_id,
                    lease_id: LEASE,
                })
                .await
                .expect("lifecycle channel open");
            actions
                .send(Action::StreamUsageMetered {
                    draft_id: DRAFT.to_owned(),
                    usage: TokenUsage {
                        prompt_tokens: Some(21),
                        completion_tokens: Some(9),
                        total_tokens: Some(30),
                        source: TokenUsageSource::Reported,
                        ..TokenUsage::default()
                    },
                })
                .await
                .expect("action channel open");
            actions
                .send(Action::ProviderTurnReadyForCommit {
                    draft_id: DRAFT.to_owned(),
                    final_text: ANSWER.to_owned(),
                    reasoning: String::new(),
                })
                .await
                .expect("action channel open");
            release.await.expect("test releases the worker");
            panic!("worker panics after its terminal");
        }
    }

    async fn phase_one(&mut self) -> Option<TurnOutcomeKind> {
        let notified = self.notified.take().expect("phase 1 waits once");
        self.run_loop.await_outcome(self.task, notified).await
    }

    /// An awaited no-op through the dispatcher: every earlier action has
    /// been reduced and its effects run when this returns.
    async fn barrier(&self) {
        let ack = self
            .dispatcher
            .dispatch_awaited(Action::RedrawRequested, &self.run_loop.signal)
            .await
            .expect("barrier acknowledged");
        assert!(matches!(ack, crate::chat::acked_session::Ack::Unchanged(_)));
    }

    fn assert_no_failure_accepted(&self) {
        let worker = self.run_loop.workers.worker(self.task).expect("worker known");
        assert!(!worker.execution_failed, "the failure gate refused the late panic");
        self.run_loop.assert_retired(self.task);
        let rows = self.run_loop.workers.snapshot();
        assert_eq!(rows.len(), 1, "one worker row");
        assert!(
            rows.iter()
                .all(|row| row.execution_exited && !row.execution_handle_attached)
        );
    }
}

#[tokio::test]
async fn a_panic_after_phase_one_keeps_the_completed_outcome_and_retires_the_lease_once() {
    let mut turn = SupervisedTurn::new();
    let (release, released) = tokio::sync::oneshot::channel();
    let worker = turn.ready_then_panic_worker(released);
    turn.supervise(worker).await;
    match turn.phase_one().await {
        Some(TurnOutcomeKind::Completed { final_text, .. }) => assert_eq!(final_text, ANSWER),
        other => panic!("phase 1 completes, got {other:?}"),
    }
    release.send(()).expect("worker waiting");
    turn.run_loop.await_exit(turn.task).await;
    turn.barrier().await;
    assert!(
        turn.run_loop.signal.consume_turn_outcome(turn.task).is_none(),
        "no failure notified"
    );
    turn.assert_no_failure_accepted();
    turn.run_loop
        .workers
        .record_execution_exited(turn.task, LEASE)
        .expect("a repeated matching exit is idempotent");
    turn.run_loop.assert_retired(turn.task);
}

#[tokio::test]
async fn a_panic_after_phase_two_keeps_the_committed_outcome_and_retires_the_lease_once() {
    let mut turn = SupervisedTurn::new();
    let (release, released) = tokio::sync::oneshot::channel();
    let worker = turn.ready_then_panic_worker(released);
    turn.supervise(worker).await;
    assert!(matches!(
        turn.phase_one().await,
        Some(TurnOutcomeKind::Completed { .. })
    ));
    dispatch_ordered_provider_turn_commit_and_wait(
        &turn.dispatcher,
        &turn.run_loop.signal,
        turn.task,
        OrderedCommitTerminal::StreamCompleted {
            draft_id: DRAFT,
            reasoning: "",
        },
        REQUEST,
        ANSWER,
        false,
        true,
    )
    .await
    .expect("phase 2 commit acknowledged");
    release.send(()).expect("worker waiting");
    turn.run_loop.await_exit(turn.task).await;
    turn.barrier().await;
    assert!(
        turn.run_loop.signal.consume_turn_outcome(turn.task).is_none(),
        "no failure notified"
    );
    assert!(
        turn.run_loop.signal.history_commit_acknowledged(turn.task),
        "the commit stays acknowledged"
    );
    turn.assert_no_failure_accepted();
}

#[tokio::test]
async fn an_aborted_worker_is_retired_without_a_failure() {
    let mut turn = SupervisedTurn::new();
    let lifecycle_tx = turn.lifecycle_tx.clone();
    let task_id = turn.task;
    turn.supervise(async move {
        lifecycle_tx
            .send(ProviderTurnLifecycleEvent::Started {
                task_id,
                lease_id: LEASE,
            })
            .await
            .expect("lifecycle channel open");
        std::future::pending::<()>().await;
    })
    .await;
    tokio::time::timeout(STEP_DEADLINE, async {
        while !turn
            .run_loop
            .workers
            .worker(task_id)
            .is_some_and(|worker| worker.execution_started && turn.run_loop.workers.execution_handle_attached(task_id))
        {
            let event = turn.run_loop.lifecycle_rx.recv().await.expect("lifecycle channel open");
            record_provider_turn_lifecycle_event(&mut turn.run_loop.workers, event);
        }
    })
    .await
    .expect("worker attached and started");
    turn.run_loop
        .workers
        .abort_execution(task_id)
        .expect("abort handle attached");
    turn.run_loop.await_exit(task_id).await;
    turn.run_loop.assert_retired(task_id);
    let worker = turn.run_loop.workers.worker(task_id).expect("worker known");
    assert!(!worker.execution_failed, "an abort is not a failure");
}

#[tokio::test]
async fn metered_usage_is_recorded_while_a_duplicate_terminal_is_refused() {
    let mut turn = SupervisedTurn::new();
    let (release, released) = tokio::sync::oneshot::channel();
    let worker = turn.ready_then_panic_worker(released);
    turn.supervise(worker).await;
    assert!(matches!(
        turn.phase_one().await,
        Some(TurnOutcomeKind::Completed { .. })
    ));
    // Usage reaches the task slot even though it was registered after phase 1.
    turn.run_loop.signal.register_turn(turn.task, DRAFT);
    let actions = turn.dispatcher.sender();
    actions
        .send(Action::StreamUsageMetered {
            draft_id: DRAFT.to_owned(),
            usage: TokenUsage {
                prompt_tokens: Some(40),
                completion_tokens: Some(2),
                total_tokens: Some(42),
                source: TokenUsageSource::Reported,
                ..TokenUsage::default()
            },
        })
        .await
        .expect("action channel open");
    for duplicate in [
        Action::ProviderTurnReadyForCommit {
            draft_id: DRAFT.to_owned(),
            final_text: "a second terminal".to_owned(),
            reasoning: String::new(),
        },
        Action::StreamFailed {
            draft_id: DRAFT.to_owned(),
            err: "a late failure".to_owned(),
            retryable: false,
        },
    ] {
        actions.send(duplicate).await.expect("action channel open");
    }
    turn.barrier().await;
    assert!(
        turn.run_loop.signal.consume_turn_outcome(turn.task).is_none(),
        "duplicate terminals are refused"
    );
    let usage = turn.run_loop.signal.consume_turn_usage(turn.task);
    assert_eq!(usage.total_tokens, Some(42), "metered usage is recorded");
    release.send(()).expect("worker waiting");
    turn.run_loop.await_exit(turn.task).await;
    turn.assert_no_failure_accepted();
}
