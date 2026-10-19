#![cfg(test)]

use crate::providers::ChatMessage;
use crate::session_worker::protocol::WorkerControlFrame;
use crate::session_worker::runner::steering::{SteeredSegment, run_segments_with_steering};
use std::future::Future;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A fake agent-loop segment: parks until cancelled for its first
/// `natural_completion_on - 1` entries, then completes on its own. This
/// exercises the steering race — cancel, unwind, re-enter, finish — without
/// a provider, config, or memory backend.
fn segment_cancellable_until(
    entries: Arc<std::sync::atomic::AtomicUsize>,
    natural_completion_on: usize,
) -> impl FnMut(
    Vec<ChatMessage>,
    CancellationToken,
    Option<String>,
) -> std::pin::Pin<Box<dyn Future<Output = SteeredSegment> + Send>> {
    move |history, cancel, _raw_steer| {
        let entries = entries.clone();
        Box::pin(async move {
            let entry = entries.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if entry < natural_completion_on {
                cancel.cancelled().await;
            }
            let output = format!("finished on entry {entry}");
            (
                history,
                Ok((output, crate::agent::turn_contracts::ToolLoopTrace::default())),
            )
        })
    }
}

#[tokio::test]
async fn steering_injects_operator_turn_and_restarts_the_segment() {
    let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(4);
    let mut steer_rx = Some(steer_rx);
    let entries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Two steers cancel and re-enter the loop; the third entry finishes.
    let mut segment = segment_cancellable_until(entries.clone(), 3);

    let driver = tokio::spawn(async move {
        steer_tx.send("pivot to X".to_string()).await.expect("first steer");
        steer_tx.send("now do Y".to_string()).await.expect("second steer");
        drop(steer_tx);
    });

    let (history, result) = run_segments_with_steering(
        vec![ChatMessage::system("sys"), ChatMessage::user("task")],
        &mut steer_rx,
        &mut segment,
    )
    .await;
    driver.await.expect("driver");

    assert!(result.is_ok(), "steered run must still complete");
    let injected = history
        .iter()
        .filter(|message| message.content.contains("[Steering instruction from operator]"))
        .map(|message| message.content.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        injected,
        vec![
            "[Steering instruction from operator] pivot to X".to_string(),
            "[Steering instruction from operator] now do Y".to_string(),
        ],
        "both steer messages must land in the worker's own history"
    );
    assert_eq!(
        entries.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "each steer must cancel and re-enter the loop"
    );
}

#[tokio::test]
async fn absent_steer_channel_runs_exactly_one_segment() {
    let mut steer_rx: Option<tokio::sync::mpsc::Receiver<String>> = None;
    let entries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let history_in = vec![ChatMessage::system("sys"), ChatMessage::user("task")];

    // Without a steer channel the segment must never be cancelled by the
    // steering race, so drive completion from outside.
    let (finished, result) = run_segments_with_steering(history_in, &mut steer_rx, |history, cancel, _raw_steer| {
        let entries = entries.clone();
        async move {
            entries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            cancel.cancel();
            cancel.cancelled().await;
            (
                history,
                Ok((
                    "done".to_string(),
                    crate::agent::turn_contracts::ToolLoopTrace::default(),
                )),
            )
        }
    })
    .await;

    assert_eq!(finished.len(), 2, "no operator turn may be injected");
    assert_eq!(result.expect("result").0, "done");
    assert_eq!(entries.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn closed_steer_channel_waits_for_natural_completion() {
    let (steer_tx, steer_rx) = tokio::sync::mpsc::channel::<String>(1);
    drop(steer_tx);
    let mut steer_rx = Some(steer_rx);
    let entries = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let (history, result) = run_segments_with_steering(
        vec![ChatMessage::user("task")],
        &mut steer_rx,
        |history, cancel, _raw_steer| {
            let entries = entries.clone();
            async move {
                entries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // A closed channel resolves immediately; the segment must
                // still be awaited to its own end rather than abandoned.
                tokio::task::yield_now().await;
                cancel.cancel();
                (
                    history,
                    Ok((
                        "natural".to_string(),
                        crate::agent::turn_contracts::ToolLoopTrace::default(),
                    )),
                )
            }
        },
    )
    .await;

    assert_eq!(history.len(), 1, "a closed channel must not inject anything");
    assert_eq!(result.expect("result").0, "natural");
    assert_eq!(entries.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn stdin_control_reader_only_accepts_steer_frames() {
    // The stdin stream carries no capability material: the sealed manifest is
    // the sole authenticated frame, so an unknown `kind` must be rejected
    // outright rather than interpreted.
    assert!(serde_json::from_str::<WorkerControlFrame>(r#"{"kind":"steer","message":"x"}"#).is_ok());
    assert!(serde_json::from_str::<WorkerControlFrame>(r#"{"kind":"grant","capability":"x"}"#).is_err());
    assert!(serde_json::from_str::<WorkerControlFrame>("not json").is_err());
}
