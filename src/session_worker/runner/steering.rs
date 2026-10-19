use crate::providers::ChatMessage;
use crate::session_worker::protocol::WorkerControlFrame;
use crate::tools::sessions_spawn::steering_instruction;
use anyhow::Result;
use std::future::Future;
use std::io::BufRead;
use tokio_util::sync::CancellationToken;

/// Start the stdin control-frame reader for a running worker.
///
/// Line 1 of stdin was already consumed as the manifest; everything after it is
/// a [`WorkerControlFrame`]. The reader runs on a **detached OS thread**, not on
/// the tokio blocking pool and not on `tokio::io::stdin`, for three reasons:
///
/// * A `tokio::io::stdin` read that is pending when the worker finishes parks a
///   blocking-pool thread that nothing can wake: the parent deliberately keeps
///   the write end of the pipe open for the whole run, and a blocking read
///   observes no `CancellationToken`. Runtime drop joins blocking threads, so
///   that read is on the critical path of exit. It does **not** hang the
///   process — `main` shuts the runtime down with `RUNTIME_SHUTDOWN_TIMEOUT`
///   (2 s, `src/main.rs`), which caps the join — but the cap is the only thing
///   that ends it, so every process-mode worker would spend those 2 seconds
///   waiting after its `WorkerResult` line was already written and its answer
///   was already in the parent's hands. A detached OS thread is torn down at
///   process exit instead, so the single-line stdout protocol still ends the
///   process the moment the result is flushed.
///
///   (This bullet, and the commit message that introduced it, originally said
///   the process would "never exit". That was wrong in exactly the way that
///   matters for a future reader deciding whether the thread is still needed:
///   the shutdown cap was already in place, so the real cost is a fixed 2 s
///   tax on every worker exit, not a wedged process. The design decision is
///   unchanged — a 2 s tax per sub-agent is reason enough — only the stated
///   reason is corrected.)
/// * It leaves the configured blocking pool (`[runtime] max_blocking_threads`,
///   the process's last implicit concurrency gate) untouched.
/// * It reads the same global buffered `std::io::Stdin` handle the manifest was
///   read from, so bytes the manifest read buffered past its newline — a steer
///   frame that arrived in the same write — are not lost.
///
/// Back-pressure, not loss: `blocking_send` parks this thread while the queue is
/// full, which stops draining the pipe, which parks the parent's writer, which
/// fills the parent's bounded queue and finally slows the steering caller down.
/// There is deliberately no wall-clock deadline anywhere on that chain: a worker
/// too busy to accept a frame is the idle detector's business, not a timeout's.
pub(super) fn spawn_stdin_control_reader(steer_tx: tokio::sync::mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                // Parent closed the pipe: no further steering is possible.
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // A malformed frame must never end the run: skip it and keep reading.
            let Ok(frame) = serde_json::from_str::<WorkerControlFrame>(line) else {
                tracing::warn!("session-worker discarded an unparseable stdin control frame");
                continue;
            };
            let WorkerControlFrame::Steer { message } = frame;
            if steer_tx.blocking_send(message).is_err() {
                break;
            }
        }
    });
}

/// Output of one cancellable agent-loop segment: the history it advanced plus
/// its result.
pub(super) type SteeredSegment = (
    Vec<ChatMessage>,
    Result<(String, crate::agent::turn_contracts::ToolLoopTrace)>,
);

/// Run an agent-loop segment, restarting it with an injected operator turn each
/// time a steer frame arrives.
///
/// This is the worker-process twin of the task-mode steering race in
/// `sessions_spawn::run_sub_agent_task`, and injects the identical
/// [`steering_instruction`] wording so both modes read the same in history.
/// A steer cancels the in-flight segment, waits for it to unwind (so the
/// history it already advanced is preserved), appends the operator turn and
/// re-enters the loop. A closed channel means no more steering, so the current
/// segment is simply awaited to its natural end.
pub(super) async fn run_segments_with_steering<F, Fut>(
    mut history: Vec<ChatMessage>,
    steer_rx: &mut Option<tokio::sync::mpsc::Receiver<String>>,
    mut run_segment: F,
) -> SteeredSegment
where
    F: FnMut(Vec<ChatMessage>, CancellationToken, Option<String>) -> Fut,
    Fut: Future<Output = SteeredSegment>,
{
    let mut raw_steer = None;
    loop {
        let cancel = CancellationToken::new();
        let segment = run_segment(history, cancel.clone(), raw_steer.take());
        tokio::pin!(segment);

        let steered = tokio::select! {
            finished = &mut segment => return finished,
            steer = next_steer(steer_rx) => steer,
        };

        match steered {
            Some(message) => {
                cancel.cancel();
                let (mut advanced, cancelled) = segment.await;
                if cancelled
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.is::<crate::runtime::turn_cleanup::CleanupIncomplete>())
                {
                    return (advanced, cancelled);
                }
                advanced.push(ChatMessage::user(steering_instruction(&message)));
                raw_steer = Some(message);
                history = advanced;
            }
            // Steer channel closed — wait for natural completion.
            None => return segment.await,
        }
    }
}

/// Await the next steer message, or never resolve when the run has no steer
/// channel at all (in-process callers and tests).
async fn next_steer(steer_rx: &mut Option<tokio::sync::mpsc::Receiver<String>>) -> Option<String> {
    match steer_rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}
