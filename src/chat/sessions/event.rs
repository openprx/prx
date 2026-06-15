//! Event bridge for live read-only attach (v1.1a).
//!
//! Background sub-agents spawned by the chat `/bg` command run a full agent loop
//! ([`crate::agent::loop_::run_tool_call_loop`]). That loop can stream its
//! incremental output and tool-call notifications over `on_delta` /
//! `on_tool_call` channels. To surface those to the chat UI **without ever
//! back-pressuring the background agent**, this module implements the decoupling
//! described in the execution plan §0.4:
//!
//! ```text
//!   background agent          drainer task              chat main loop
//!   (run_tool_call_loop)   (one per session)        (single SessionEvent rx)
//!
//!   on_delta  ──send().await──▶  raw_delta_rx  ─┐
//!                                                ├─ try_send ─▶ event_tx ──▶ ring
//!   on_tool_call ─send().await─▶  raw_tool_rx  ─┘   (drop-on-full + truncated)
//! ```
//!
//! Key invariants (iron law + §0.4):
//! - The background agent only ever `.send().await`s onto the **middle channel**.
//!   A dedicated drainer task is always receiving from it, so the agent never
//!   blocks (no back-pressure reaches the agent).
//! - The drainer forwards to the chat main loop with **`try_send`**: if the main
//!   loop is slow and the [`SessionEvent`] channel is full, the event is dropped
//!   and the receiving [`SessionRing`] is flagged `truncated`. Soft degradation
//!   happens *only* on this drainer → main-loop hop, never back to the agent.
//! - The [`SessionRing`] is written **only** on the chat main loop (single
//!   consumer); neither the agent nor the drainer touches it. No lock is held
//!   across an `.await`.
//!
//! Note (§0.4 wording): `on_delta` is **not** native provider tokens — it is the
//! loop's incremental output text. We display "incremental loop output", not
//! "real-time tokens".

use super::id::SessionId;
use crate::agent::loop_::{SpawnEventSink, ToolCallNotification};
use std::collections::VecDeque;
use tokio::sync::mpsc;

/// Capacity of the middle channels handed to the background agent. The drainer
/// drains these continuously, so this only needs to absorb short bursts between
/// drainer wake-ups; it never causes the agent to block for long.
pub const RAW_CHANNEL_CAPACITY: usize = 256;

/// Capacity of the chat main loop's single [`SessionEvent`] channel. When full,
/// the drainer drops events (soft degradation) and flags the ring truncated.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Default per-session ring buffer line capacity for attach display.
pub const DEFAULT_RING_CAPACITY: usize = 500;

/// An event emitted by a background session, tagged with its [`SessionId`] so the
/// chat main loop can route it to the right [`SessionRing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Incremental loop output text (from `on_delta`).
    Delta { id: SessionId, text: String },
    /// A human-readable tool-call notification (from `on_tool_call`).
    ToolCall { id: SessionId, summary: String },
}

impl SessionEvent {
    /// The session this event belongs to.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        match self {
            Self::Delta { id, .. } | Self::ToolCall { id, .. } => id,
        }
    }
}

/// The chat-side sink that owns the producer end of the chat main loop's single
/// [`SessionEvent`] channel and turns it into the library-level
/// [`SpawnEventSink`] that `tools::sessions_spawn` understands.
///
/// `tools` is a *library* module and must not depend on `chat`, so the wiring is
/// inverted: this chat-side type builds a closure (capturing `event_tx`) that,
/// per run id, creates the middle channels and spawns the drainer. The library
/// only invokes that closure. Cloning is cheap (an `mpsc::Sender` clone).
#[derive(Clone)]
pub struct SessionEventSink {
    event_tx: mpsc::Sender<SessionEvent>,
}

impl SessionEventSink {
    /// Build a sink/receiver pair. The chat main loop keeps the `Receiver`
    /// (it must be the single consumer — never clone or share it); the
    /// [`SessionEventSink`] is turned into a [`SpawnEventSink`] for the tool.
    #[must_use]
    pub fn channel() -> (Self, mpsc::Receiver<SessionEvent>) {
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        (Self { event_tx }, event_rx)
    }

    /// Build the library-level [`SpawnEventSink`] to hand to the spawn tool.
    ///
    /// The returned sink, when invoked by `sessions_spawn` for a given run id,
    /// creates that run's middle channels, spawns its drainer (tagged with the
    /// run's [`SessionId`]), and returns the `on_delta` / `on_tool_call`
    /// senders for `run_tool_call_loop`.
    #[must_use]
    pub fn into_spawn_sink(self) -> SpawnEventSink {
        let event_tx = self.event_tx;
        SpawnEventSink::new(move |run_id: &str| {
            let id = SessionId::from_run_id(run_id);
            let (raw_delta_tx, raw_delta_rx) = mpsc::channel::<String>(RAW_CHANNEL_CAPACITY);
            let (raw_tool_tx, raw_tool_rx) = mpsc::channel::<ToolCallNotification>(RAW_CHANNEL_CAPACITY);
            spawn_drainer(id, raw_delta_rx, raw_tool_rx, event_tx.clone());
            (raw_delta_tx, raw_tool_tx)
        })
    }

    /// Test helper: create a run's middle channels + drainer directly, mirroring
    /// what [`Self::into_spawn_sink`]'s closure does, so unit tests can exercise
    /// the drainer without going through the library indirection.
    #[cfg(test)]
    #[must_use]
    pub fn attach_run(&self, id: SessionId) -> (mpsc::Sender<String>, mpsc::Sender<ToolCallNotification>) {
        let (raw_delta_tx, raw_delta_rx) = mpsc::channel::<String>(RAW_CHANNEL_CAPACITY);
        let (raw_tool_tx, raw_tool_rx) = mpsc::channel::<ToolCallNotification>(RAW_CHANNEL_CAPACITY);
        spawn_drainer(id, raw_delta_rx, raw_tool_rx, self.event_tx.clone());
        (raw_delta_tx, raw_tool_tx)
    }
}

/// Spawn the drainer task for one session.
///
/// It continuously consumes both middle channels (so the background agent's
/// `.send().await` never blocks), converts each item into a [`SessionEvent`],
/// and `try_send`s it to the chat main loop. On a full main-loop channel the
/// event is dropped (the ring will be flagged `truncated` by the main loop on
/// the next successful event, or it stops growing — soft degradation only).
fn spawn_drainer(
    id: SessionId,
    mut raw_delta_rx: mpsc::Receiver<String>,
    mut raw_tool_rx: mpsc::Receiver<ToolCallNotification>,
    event_tx: mpsc::Sender<SessionEvent>,
) {
    tokio::spawn(async move {
        let mut delta_open = true;
        let mut tool_open = true;
        while delta_open || tool_open {
            tokio::select! {
                delta = raw_delta_rx.recv(), if delta_open => {
                    match delta {
                        Some(text) => forward(&event_tx, SessionEvent::Delta { id: id.clone(), text }),
                        None => delta_open = false,
                    }
                }
                tool = raw_tool_rx.recv(), if tool_open => {
                    match tool {
                        Some(notif) => {
                            if let Some(summary) = summarize_tool_call(&notif) {
                                forward(&event_tx, SessionEvent::ToolCall { id: id.clone(), summary });
                            }
                        }
                        None => tool_open = false,
                    }
                }
            }
        }
    });
}

/// Forward one event to the chat main loop, dropping on a full/closed channel
/// (soft degradation — never blocks the drainer, never back-pressures the agent).
fn forward(event_tx: &mpsc::Sender<SessionEvent>, event: SessionEvent) {
    match event_tx.try_send(event) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            // Main loop is behind; drop the event (soft degradation). The drainer
            // never writes the ring, so it simply drops here — back-pressure
            // never reaches the background agent. The ring separately flags
            // `truncated` when its own line capacity overflows.
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            // Chat main loop has shut down its receiver; nothing more to do.
        }
    }
}

/// Render a [`ToolCallNotification`] into a one-line human summary for attach
/// display, or `None` for notifications we do not surface (progress ticks).
fn summarize_tool_call(notif: &ToolCallNotification) -> Option<String> {
    match notif {
        ToolCallNotification::Started { name, args_summary } => {
            if args_summary.is_empty() {
                Some(format!("→ {name}"))
            } else {
                Some(format!("→ {name}({args_summary})"))
            }
        }
        ToolCallNotification::Finished {
            name,
            success,
            duration_ms,
        } => {
            let mark = if *success { "ok" } else { "fail" };
            Some(format!("✓ {name} [{mark} {duration_ms}ms]"))
        }
        // Progress ticks are noise for the attach view; the status line already
        // conveys running state.
        ToolCallNotification::Progress { .. } => None,
    }
}

/// A bounded ring buffer of a session's most recent output lines, written
/// **only** on the chat main loop (single consumer). When full, the oldest line
/// is dropped and `truncated` is set so the UI can show `[output truncated]`.
#[derive(Debug)]
pub struct SessionRing {
    buf: VecDeque<String>,
    cap: usize,
    truncated: bool,
    /// Lines already drained by an attached follower (the index past which new
    /// lines are "unseen"). Lets `/attach` print only newly-appended lines.
    drained: usize,
}

impl SessionRing {
    /// Build a ring with the given line capacity (clamped to at least 1).
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: VecDeque::new(),
            cap: cap.max(1),
            truncated: false,
            drained: 0,
        }
    }

    /// Append one line, dropping the oldest if at capacity (and flagging
    /// `truncated`). Pure main-loop state; no lock, no await.
    pub fn push(&mut self, line: String) {
        if self.buf.len() >= self.cap {
            self.buf.pop_front();
            self.truncated = true;
            // The drained cursor counts from the front; a popped front shifts it.
            self.drained = self.drained.saturating_sub(1);
        }
        self.buf.push_back(line);
    }

    /// Whether any line has been dropped due to capacity.
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Number of lines currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the ring is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Take the lines appended since the last [`Self::drain_new`] call (the
    /// "follow" delta for a live attach). Advances the drained cursor.
    pub fn drain_new(&mut self) -> Vec<String> {
        let new: Vec<String> = self.buf.iter().skip(self.drained).cloned().collect();
        self.drained = self.buf.len();
        new
    }

    /// Reset the drained cursor to the start so a fresh `/attach` replays the
    /// full retained window (then follows new lines via [`Self::drain_new`]).
    pub const fn rewind(&mut self) {
        self.drained = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_pushes_and_drains_new() {
        let mut ring = SessionRing::with_capacity(10);
        ring.push("a".into());
        ring.push("b".into());
        assert_eq!(ring.drain_new(), vec!["a".to_string(), "b".to_string()]);
        // Nothing new yet.
        assert!(ring.drain_new().is_empty());
        ring.push("c".into());
        assert_eq!(ring.drain_new(), vec!["c".to_string()]);
    }

    #[test]
    fn ring_truncates_oldest_when_full() {
        let mut ring = SessionRing::with_capacity(2);
        ring.push("1".into());
        ring.push("2".into());
        assert!(!ring.is_truncated());
        ring.push("3".into());
        assert!(ring.is_truncated());
        assert_eq!(ring.len(), 2);
        // Oldest ("1") dropped.
        let all = ring.drain_new();
        assert_eq!(all, vec!["2".to_string(), "3".to_string()]);
    }

    #[test]
    fn ring_rewind_replays_window() {
        let mut ring = SessionRing::with_capacity(10);
        ring.push("a".into());
        ring.push("b".into());
        let _ = ring.drain_new();
        ring.rewind();
        assert_eq!(ring.drain_new(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn capacity_clamped_to_at_least_one() {
        let mut ring = SessionRing::with_capacity(0);
        ring.push("x".into());
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn session_event_carries_id() {
        let id = SessionId::from_run_id("run-1");
        let ev = SessionEvent::Delta {
            id: id.clone(),
            text: "hi".into(),
        };
        assert_eq!(ev.session_id(), &id);
    }

    #[test]
    fn summarize_started_and_finished() {
        let started = ToolCallNotification::Started {
            name: "read".into(),
            args_summary: "file=x".into(),
        };
        assert_eq!(summarize_tool_call(&started), Some("→ read(file=x)".to_string()));
        let started_noargs = ToolCallNotification::Started {
            name: "ls".into(),
            args_summary: String::new(),
        };
        assert_eq!(summarize_tool_call(&started_noargs), Some("→ ls".to_string()));
        let finished = ToolCallNotification::Finished {
            name: "read".into(),
            success: true,
            duration_ms: 12,
        };
        assert_eq!(summarize_tool_call(&finished), Some("✓ read [ok 12ms]".to_string()));
        let progress = ToolCallNotification::Progress {
            iteration: 1,
            max_iterations: 5,
        };
        assert_eq!(summarize_tool_call(&progress), None);
    }

    #[tokio::test]
    async fn drainer_forwards_delta_and_tool_events() {
        let (sink, mut rx) = SessionEventSink::channel();
        let id = SessionId::from_run_id("run-d");
        let (delta_tx, tool_tx) = sink.attach_run(id.clone());

        delta_tx.send("hello".to_string()).await.expect("test: send delta");
        tool_tx
            .send(ToolCallNotification::Started {
                name: "read".into(),
                args_summary: String::new(),
            })
            .await
            .expect("test: send tool");
        drop(delta_tx);
        drop(tool_tx);

        let mut got_delta = false;
        let mut got_tool = false;
        // Collect both events (order between the two channels is not guaranteed).
        for _ in 0..2 {
            match rx.recv().await {
                Some(SessionEvent::Delta { id: eid, text }) => {
                    assert_eq!(eid, id);
                    assert_eq!(text, "hello");
                    got_delta = true;
                }
                Some(SessionEvent::ToolCall { id: eid, summary }) => {
                    assert_eq!(eid, id);
                    assert_eq!(summary, "→ read");
                    got_tool = true;
                }
                None => break,
            }
        }
        assert!(got_delta, "delta event forwarded");
        assert!(got_tool, "tool event forwarded");
    }

    #[tokio::test]
    async fn drainer_never_blocks_agent_when_main_loop_stalls() {
        // Tiny event channel; main loop never drains. The agent-side senders
        // must still complete (drainer keeps draining the middle channel,
        // try_send drops on full → no back-pressure to the "agent").
        let (sink, _rx) = SessionEventSink::channel();
        let id = SessionId::from_run_id("run-flood");
        let (delta_tx, _tool_tx) = sink.attach_run(id);
        // Send far more than EVENT_CHANNEL_CAPACITY; each send must complete
        // promptly because the drainer is always receiving and dropping.
        for i in 0..(EVENT_CHANNEL_CAPACITY * 4) {
            // A bounded wait proves no permanent block; the middle channel may
            // briefly fill but the drainer empties it continuously.
            tokio::time::timeout(std::time::Duration::from_secs(5), delta_tx.send(format!("line {i}")))
                .await
                .expect("test: agent send must not block forever")
                .expect("test: drainer keeps middle channel open");
        }
    }
}
