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

/// An event emitted by a child session, tagged with its [`SessionId`] so the
/// chat main loop can route it to the right [`SessionRing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Incremental loop output text (from `on_delta`).
    Delta { id: SessionId, text: String },
    /// A human-readable tool-call notification (from `on_tool_call`).
    ToolCall { id: SessionId, summary: String },
    /// The drainer had to drop one or more events for this session because the
    /// chat main loop's event channel was full (soft degradation, §0.4). Carries
    /// no payload — it only tells the main loop to flag the session's ring
    /// `truncated` so `/attach` shows `[output truncated]`. Emitted lazily on the
    /// next successful forward after a drop (the drainer never blocks to send it).
    Truncated { id: SessionId },
    /// The child session suspended on a tool call that needs an operator
    /// approval decision (NeedsInput). `prompt` summarises what is awaiting
    /// approval (tool name + a short argument digest). The main loop surfaces a
    /// non-intrusive `/approve` / `/deny` hint and the session's status flips to
    /// `❓ needs-input`. Sent directly on the [`SessionEventSink`] channel (not
    /// via the drainer) so it is never dropped under output back-pressure.
    NeedsInput { id: SessionId, prompt: String },
    /// A previously [`NeedsInput`](Self::NeedsInput) session resumed — the
    /// operator decided (`/approve` / `/deny`) or the approval timed out, so the
    /// suspend banner can be cleared. Sent directly on the sink channel.
    Resumed { id: SessionId },
}

impl SessionEvent {
    /// The session this event belongs to.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        match self {
            Self::Delta { id, .. }
            | Self::ToolCall { id, .. }
            | Self::Truncated { id }
            | Self::NeedsInput { id, .. }
            | Self::Resumed { id } => id,
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

    /// Clone the underlying [`SessionEvent`] sender.
    ///
    /// Used by the NeedsInput approval path (`super::approval`): its resolver
    /// emits [`SessionEvent::NeedsInput`] / [`SessionEvent::Resumed`] **directly**
    /// on this channel (not via the per-session drainer), so suspend/resume
    /// banners are never dropped under output back-pressure.
    #[must_use]
    pub fn event_sender(&self) -> mpsc::Sender<SessionEvent> {
        self.event_tx.clone()
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

    /// Create a run's middle channels + drainer directly, mirroring what
    /// [`Self::into_spawn_sink`]'s closure does for agents.
    ///
    /// Background shell sessions (v2) call this to obtain `on_delta`-style
    /// senders that stream their stdout/stderr through the same decoupled drainer
    /// → ring-buffer path agents use, so live `/attach` and `/logs` work
    /// uniformly across both kinds. Unit tests also use it to exercise the
    /// drainer without the library indirection.
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
        // Set whenever a `forward` drops an event because the main-loop channel
        // was full. On the next successful forward we first emit a
        // `SessionEvent::Truncated` marker so the main loop can flag the ring
        // `truncated` (and `/attach` shows `[output truncated]`). This is the
        // only way the drainer signals a drop: it never blocks, never holds a
        // lock, never touches the ring, and never back-pressures the agent.
        let mut dropped = false;
        while delta_open || tool_open {
            tokio::select! {
                delta = raw_delta_rx.recv(), if delta_open => {
                    match delta {
                        Some(text) => forward(&event_tx, &id, SessionEvent::Delta { id: id.clone(), text }, &mut dropped),
                        None => delta_open = false,
                    }
                }
                tool = raw_tool_rx.recv(), if tool_open => {
                    match tool {
                        Some(notif) => {
                            if let Some(summary) = summarize_tool_call(&notif) {
                                forward(&event_tx, &id, SessionEvent::ToolCall { id: id.clone(), summary }, &mut dropped);
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
///
/// `dropped` carries the "we dropped at least one event since the last
/// successful send" flag across calls. When set, this function first tries to
/// emit a [`SessionEvent::Truncated`] marker (a single, cheap signal that the
/// main loop turns into a `truncated` flag on the session ring). The marker
/// itself is sent with `try_send`: if the channel is still full it stays
/// pending (`dropped` remains set) and we simply skip this event too — the
/// drainer never blocks.
fn forward(event_tx: &mpsc::Sender<SessionEvent>, id: &SessionId, event: SessionEvent, dropped: &mut bool) {
    // If we owe a truncation marker, try to send it before the real event so the
    // `[output truncated]` indicator precedes the next visible output.
    if *dropped {
        match event_tx.try_send(SessionEvent::Truncated { id: id.clone() }) {
            Ok(()) => *dropped = false,
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Still backed up; keep `dropped` set and drop this event too.
                return;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
    match event_tx.try_send(event) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            // Main loop is behind; drop the event (soft degradation) and remember
            // it so the next successful forward emits a `Truncated` marker. The
            // drainer never writes the ring and never blocks — back-pressure
            // never reaches the background agent.
            *dropped = true;
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
    last_pushed_at: Option<chrono::DateTime<chrono::Utc>>,
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
            last_pushed_at: None,
            drained: 0,
        }
    }

    /// Append one line, dropping the oldest if at capacity (and flagging
    /// `truncated`). Pure main-loop state; no lock, no await.
    pub fn push(&mut self, line: String) {
        self.push_at(line, chrono::Utc::now());
    }

    /// Append one line with an explicit timestamp. Production uses [`Self::push`];
    /// tests use this to pin idle-warning decisions without sleeping.
    pub fn push_at(&mut self, line: String, at: chrono::DateTime<chrono::Utc>) {
        if self.buf.len() >= self.cap {
            self.buf.pop_front();
            self.truncated = true;
            // The drained cursor counts from the front; a popped front shifts it.
            self.drained = self.drained.saturating_sub(1);
        }
        self.buf.push_back(line);
        self.last_pushed_at = Some(at);
    }

    #[must_use]
    pub const fn last_pushed_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.last_pushed_at
    }

    /// Whether any line has been dropped — either by this ring's own line
    /// capacity overflow, or by the drainer dropping events on a full main-loop
    /// channel (signalled via [`SessionEvent::Truncated`], applied with
    /// [`Self::mark_truncated`]).
    #[must_use]
    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Flag the ring `truncated` because the drainer dropped one or more events
    /// upstream (a full main-loop channel), not this ring's own capacity. Pure
    /// main-loop state; no lock, no await. Idempotent.
    pub const fn mark_truncated(&mut self) {
        self.truncated = true;
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

    /// Snapshot up to `max` of the most recent retained lines **without** touching
    /// the drained cursor (read-only). Used by `/logs` to dump a session's buffer
    /// while a concurrent live `/attach` follow keeps streaming uninterrupted.
    #[must_use]
    pub fn recent_lines(&self, max: usize) -> Vec<String> {
        let start = self.buf.len().saturating_sub(max);
        self.buf.iter().skip(start).cloned().collect()
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
        assert!(ring.last_pushed_at().is_some());
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
                Some(SessionEvent::Truncated { .. }) => {
                    // No drop expected in this small, fully-drained scenario.
                    panic!("test: unexpected truncation marker without a drop");
                }
                Some(SessionEvent::NeedsInput { .. } | SessionEvent::Resumed { .. }) => {
                    // Control signals are emitted only by the approval resolver,
                    // never by `forward`; the stream-bridge test cannot produce them.
                    panic!("test: unexpected approval control signal from forward()");
                }
                None => break,
            }
        }
        assert!(got_delta, "delta event forwarded");
        assert!(got_tool, "tool event forwarded");
    }

    #[test]
    fn forward_emits_truncated_marker_after_a_drop() {
        // Capacity-1 channel: fill it, force a drop, then drain and confirm the
        // next successful forward is preceded by a `Truncated` marker (P1).
        let (event_tx, mut rx) = mpsc::channel::<SessionEvent>(1);
        let id = SessionId::from_run_id("run-trunc");
        let mut dropped = false;

        // 1) First forward fills the channel (succeeds, no pending drop).
        forward(
            &event_tx,
            &id,
            SessionEvent::Delta {
                id: id.clone(),
                text: "a".into(),
            },
            &mut dropped,
        );
        assert!(!dropped, "first send fits");

        // 2) Channel is now full → this forward drops and records it.
        forward(
            &event_tx,
            &id,
            SessionEvent::Delta {
                id: id.clone(),
                text: "b".into(),
            },
            &mut dropped,
        );
        assert!(dropped, "second send dropped on full channel");

        // 3) Drain the one buffered event so the channel has room again.
        assert_eq!(
            rx.try_recv().expect("test: first buffered event"),
            SessionEvent::Delta {
                id: id.clone(),
                text: "a".into(),
            }
        );

        // 4) Next forward must first emit a Truncated marker, then the event.
        forward(
            &event_tx,
            &id,
            SessionEvent::Delta {
                id: id.clone(),
                text: "c".into(),
            },
            &mut dropped,
        );
        // The marker was sent first (channel had room for exactly one), which
        // cleared the pending-drop flag; then the real "c" event found the
        // channel full again and was dropped, re-arming the flag.
        assert!(dropped, "the 'c' event dropped on the now-full channel, re-arming");
        assert_eq!(
            rx.try_recv().expect("test: truncated marker first"),
            SessionEvent::Truncated { id: id.clone() }
        );
        // And nothing else is buffered ("c" was dropped).
        assert!(rx.try_recv().is_err(), "no further event buffered");
    }

    #[test]
    fn ring_mark_truncated_flags_without_pushing() {
        let mut ring = SessionRing::with_capacity(10);
        ring.push("a".into());
        assert!(!ring.is_truncated());
        ring.mark_truncated();
        assert!(ring.is_truncated(), "marker flags truncated");
        assert_eq!(ring.len(), 1, "no line was added by the marker");
        // Idempotent.
        ring.mark_truncated();
        assert!(ring.is_truncated());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drainer_signals_truncation_when_main_loop_stalls() {
        // End-to-end through the real drainer task: a slow consumer drains one
        // event at a time with a pause, so the bounded event channel fills and
        // the drainer must drop. Because the consumer keeps draining (creating
        // room) while the producer keeps feeding (backlog remains), the drainer's
        // next successful forward emits a `Truncated` marker. We assert at least
        // one marker reaches the consumer. The consumer always stays slower than
        // the producer's burst, guaranteeing drops without relying on exact
        // timing windows.
        let (sink, mut rx) = SessionEventSink::channel();
        let id = SessionId::from_run_id("run-trunc-flood");
        let (delta_tx, _tool_tx) = sink.attach_run(id.clone());

        // Producer: flood far beyond channel capacity, then close.
        let producer = tokio::spawn(async move {
            for i in 0..(EVENT_CHANNEL_CAPACITY * 8) {
                if delta_tx.send(format!("line {i}")).await.is_err() {
                    break;
                }
            }
            drop(delta_tx);
        });

        // Consumer: drain slowly so the channel overflows (drops) but still keeps
        // making room, so a marker is eventually forwarded.
        let mut saw_truncated = false;
        let mut received = 0usize;
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await {
                Ok(Some(SessionEvent::Truncated { id: eid })) => {
                    assert_eq!(eid, id);
                    saw_truncated = true;
                    break;
                }
                Ok(Some(_)) => {
                    received += 1;
                    // Pause periodically to let the channel fill behind us.
                    if received % 16 == 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        producer.await.expect("test: producer joins");
        assert!(saw_truncated, "a truncation marker must be surfaced after drops");
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
