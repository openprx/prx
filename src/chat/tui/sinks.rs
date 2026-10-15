use super::TuiState;
use parking_lot::Mutex;

// ── P3-4: TuiMirrorSink adapter ─────────────────────────────────────────────

/// Adapter that lets a shared `Arc<Mutex<TuiState>>` be plugged into the
/// channel-side `UiActor` as a [`TuiMirrorSink`].
///
/// `channels::terminal` lives in the library crate and cannot name
/// `TuiState` directly — see the trait doc for the seam design. This adapter
/// is the binary-side glue: each trait method maps onto the corresponding
/// `TuiState` mutator behind the existing parking_lot mutex. No new
/// `ConversationLine` variant is introduced, so the renderer's existing
/// view layer is untouched.
pub struct TuiStateMirrorSink {
    pub(super) state: std::sync::Arc<Mutex<TuiState>>,
}

impl TuiStateMirrorSink {
    pub const fn new(state: std::sync::Arc<Mutex<TuiState>>) -> Self {
        Self { state }
    }
}

impl crate::channels::terminal::TuiMirrorSink for TuiStateMirrorSink {
    fn push_assistant(&self, content: &str) {
        self.state.lock().push_assistant_message(content);
    }
    fn push_system(&self, content: &str) {
        self.state.lock().push_system_message(content);
    }
    fn push_tool_started(&self, tool_name: &str, args_full: &str) {
        self.state.lock().push_tool_result_started(tool_name, args_full);
    }
    fn mark_tool_finished(&self, tool_name: &str, success: bool, duration_ms: u64) -> bool {
        self.state
            .lock()
            .mark_last_tool_result_finished(tool_name, success, duration_ms, None)
    }
    // ── P3-5: streaming bridge ─────────────────────────────────────────
    fn start_stream(&self, draft_id: &str) {
        let _ = self.state.lock().start_stream(draft_id);
    }
    fn update_stream(&self, draft_id: &str, accumulated: &str, version: u64) {
        let _ = self.state.lock().update_stream(draft_id, accumulated, version);
    }
    fn finalize_stream(&self, draft_id: &str, final_text: &str) {
        self.state.lock().finalize_stream(draft_id, final_text);
    }
    fn cancel_stream(&self, draft_id: &str) {
        self.state.lock().cancel_stream(draft_id);
    }
}

// ── S4-A Commit 5: SnapshotDispatcherSink ───────────────────────────────────

/// Pure-mode-only Sink — UiActor events are translated into Redux Actions through
/// `ChatDispatcher` and take the reducer's single persistence path; `chat_mirror` is
/// no longer written.
///
/// Relation to [`TuiStateMirrorSink`]: in Pure mode the two are mutually exclusive.
/// The main LLM turn path in Pure mode dispatches
/// `Action::TurnStarted` / `Action::StreamChunkReceived` / `Action::StreamCompleted` /
/// `Action::ToolStarted` / `Action::ToolFinished` to the reducer directly from
/// `drive_start_turn_stream`; the equivalent UiEvents that reach UiActor are only a
/// channel-layer mirror broadcast, and re-dispatching them as Actions would make the
/// reducer double-write conversation_lines / draft. This Sink is therefore a **no-op**
/// in its main body (trace only) — the real writer of the reducer is the driver, and
/// the Sink merely acks UiActor events passively.
///
/// `push_system` is the only fallback path towards the chat::run main loop:
/// `UiEvent::ToolProgress` / `UiEvent::DraftCancelled` are translated by UiActor into
/// "step N/M" / "(cancelled)" system notices, and this Sink re-dispatches them as
/// `Action::SystemMessageAdded` so the reducer pushes the message into the UI ledger.
pub struct SnapshotDispatcherSink {
    pub(super) dispatcher: crate::chat::dispatcher::ChatDispatcher,
}

impl SnapshotDispatcherSink {
    pub const fn new(dispatcher: crate::chat::dispatcher::ChatDispatcher) -> Self {
        Self { dispatcher }
    }
}

impl crate::channels::terminal::TuiMirrorSink for SnapshotDispatcherSink {
    fn push_assistant(&self, content: &str) {
        // In Pure mode assistant text is pushed to conversation_lines by the driver's
        // Action::StreamCompleted through the reducer. UiActor's push_assistant is a
        // channel-layer side mirror; dispatching it again would duplicate lines, so we
        // only leave a trace here for observability.
        tracing::trace!(
            site = "snapshot_sink.push_assistant",
            chars = content.chars().count(),
            "Pure mode ignores the UiActor side path (reducer is the single source)"
        );
    }
    fn push_system(&self, content: &str) {
        // System messages (ToolProgress / DraftCancelled, ...) are written into the UI
        // ledger through the reducer as the single source.
        let _ = self.dispatcher.dispatch_or_log(
            crate::chat::action::Action::SystemMessageAdded {
                text: content.to_string(),
            },
            "snapshot_sink.push_system",
        );
    }
    fn push_tool_started(&self, tool_name: &str, args_full: &str) {
        // The driver already dispatched Action::ToolStarted; Pure mode ignores the
        // UiActor side path.
        tracing::trace!(
            site = "snapshot_sink.push_tool_started",
            tool = tool_name,
            args_len = args_full.chars().count(),
            "Pure mode ignores the UiActor side path (driver already dispatched ToolStarted)"
        );
    }
    fn mark_tool_finished(&self, tool_name: &str, success: bool, duration_ms: u64) -> bool {
        // The driver already dispatched Action::ToolFinished. Returning false tells
        // UiActor that this sink did not update any card "on its own" — the real update
        // happens inside the reducer, triggered by the Action the driver dispatched.
        // UiActor no longer relies on this return value for a fallback (real path).
        tracing::trace!(
            site = "snapshot_sink.mark_tool_finished",
            tool = tool_name,
            success,
            duration_ms,
            "Pure mode ignores the UiActor side path (driver already dispatched ToolFinished)"
        );
        false
    }
    fn start_stream(&self, draft_id: &str) {
        tracing::trace!(
            site = "snapshot_sink.start_stream",
            draft_id,
            "Pure mode ignores the UiActor side path (driver already dispatched TurnStarted)"
        );
    }
    fn update_stream(&self, draft_id: &str, accumulated: &str, version: u64) {
        tracing::trace!(
            site = "snapshot_sink.update_stream",
            draft_id,
            version,
            chars = accumulated.chars().count(),
            "Pure mode ignores the UiActor side path (driver already dispatched StreamChunkReceived)"
        );
    }
    fn finalize_stream(&self, draft_id: &str, final_text: &str) {
        tracing::trace!(
            site = "snapshot_sink.finalize_stream",
            draft_id,
            chars = final_text.chars().count(),
            "Pure mode ignores the UiActor side path (driver already dispatched StreamCompleted)"
        );
    }
    fn cancel_stream(&self, draft_id: &str) {
        tracing::trace!(
            site = "snapshot_sink.cancel_stream",
            draft_id,
            "Pure mode ignores the UiActor side path (driver already dispatched StreamCancelled)"
        );
    }
}
