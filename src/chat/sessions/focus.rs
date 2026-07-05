//! Input-focus target + session-switcher sub-mode for the chat TUI (v1.1b).
//!
//! These are **pure display/decision types** with no async, no locks, and no
//! registry access, so they can live in the synchronous TUI key thread
//! (`dispatch_global_key` over `Arc<parking_lot::Mutex<TuiState>>`) and be
//! unit-tested in isolation.
//!
//! Two concerns:
//!
//! - [`FocusTarget`] — where plain text + Enter is routed. `Main` (the default)
//!   sends to the main chat agent; `Session { seq }` routes the text as a
//!   *steer* to the attached child session (head footgun: input target
//!   ambiguity — see plan §0.2.1 A). The target is shown in the prompt with a
//!   colour **and** a glyph so it is never colour-only (colour-blind / no-color
//!   safe).
//!
//! - [`SwitcherState`] — the Ctrl+G session switcher overlay. A small, bottom-
//!   chrome popup (never an alternate screen) listing child TUI sessions; the
//!   user navigates and attaches without typing a command.
//!
//! [`resolve_esc`] is the pure decision function for the context-dependent Esc
//! key: it preserves the existing "non-empty input → clear" muscle memory and
//! only detaches when the input is empty *and* a session is focused (plan
//! §v1.1 P0-8).

use super::model::{ManagedKind, ManagedSessionView, ManagedStatus, elapsed_seconds_between, format_elapsed_compact};
use chrono::{DateTime, Utc};

/// Where plain text + Enter is currently routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusTarget {
    /// Default: input goes to the main chat agent loop.
    #[default]
    Main,
    /// Input is routed as a *steer* to the attached child TUI session `#seq`.
    Session { seq: u64 },
    /// Read-only conversation transcript viewer. It reuses the child viewport but
    /// never routes submitted text as a steer to a managed session.
    Transcript,
    /// Foreground tool approval prompt. It is a child TUI surface but never a
    /// steerable managed session.
    Approval,
    /// Read-only workspace diff viewer. It is a child TUI surface but never a
    /// steerable managed session.
    Diff,
}

impl FocusTarget {
    /// Whether a child TUI session currently has input focus.
    #[must_use]
    pub const fn is_session(self) -> bool {
        matches!(self, Self::Session { .. })
    }

    /// Whether any child viewport is currently focused.
    #[must_use]
    pub const fn is_child_view(self) -> bool {
        matches!(
            self,
            Self::Session { .. } | Self::Transcript | Self::Approval | Self::Diff
        )
    }

    /// The focused session's display sequence `#N`, if any.
    #[must_use]
    pub const fn session_seq(self) -> Option<u64> {
        match self {
            Self::Main | Self::Transcript | Self::Approval | Self::Diff => None,
            Self::Session { seq } => Some(seq),
        }
    }
}

/// Display-only foreground tool approval request.
///
/// The actual execution gate remains [`crate::chat::dispatcher::ApprovalRouter`].
/// This view exists only so the TUI can show the pending request and route a
/// human decision back as `Action::ToolApprovalReceived`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingToolApprovalView {
    pub tool_id: String,
    pub name: String,
    pub args: String,
}

/// One row in the Ctrl+G session switcher. A plain display snapshot (no async,
/// no registry handle) so the synchronous key thread can render and navigate it.
#[derive(Debug, Clone, PartialEq)]
pub struct SwitcherEntry {
    /// Display sequence `#N`.
    pub seq: u64,
    /// Stable lowercase kind label (`agent` / `shell` / `pty`).
    pub kind: &'static str,
    /// Stable lowercase origin label (`user` / `model`), so the operator can
    /// tell which sessions the model started for itself (v5, §17).
    pub origin: &'static str,
    /// Stable lowercase status label (`running` / `completed` / …).
    pub status: &'static str,
    /// Task / command title (already truncated by the projection).
    pub title: String,
    /// Session start timestamp used for elapsed display.
    pub created_at: DateTime<Utc>,
    /// Live snapshot time for running sessions, final timestamp for terminal sessions.
    pub updated_at: DateTime<Utc>,
    pub token_usage_records: Vec<crate::chat::session::SessionTokenUsageRecord>,
    /// Display-only stale interactive warning. This never changes routing or
    /// lifecycle; it only marks detached shell/PTY entries that have been idle
    /// past the cleanup policy's warning threshold.
    pub idle_warning: bool,
}

/// Pure render snapshot for the focused line-oriented child session viewport
/// (P2). The chat main loop owns the live [`SessionRing`](super::event::SessionRing);
/// the TUI receives only this bounded, cloneable view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSessionView {
    /// Display sequence `#N`.
    pub seq: u64,
    /// Stable lowercase kind label (`agent` / `shell`).
    pub kind: String,
    /// Dynamic task / command title.
    pub title: String,
    /// Retained output lines for the viewport.
    pub lines: Vec<String>,
    /// Whether retained output was truncated upstream.
    pub truncated: bool,
    /// Lines scrolled up from tail. `0` means pinned to newest output.
    pub scroll_offset: usize,
}

impl ActiveSessionView {
    /// Maximum legal scroll offset for a viewport body of `visible_rows` lines.
    #[must_use]
    pub const fn max_scroll_offset(&self, visible_rows: usize) -> usize {
        self.lines.len().saturating_sub(visible_rows)
    }

    /// Return a copy with scroll offset clamped for the supplied viewport height.
    #[must_use]
    pub fn clamped_for_height(mut self, visible_rows: usize) -> Self {
        let max = self.max_scroll_offset(visible_rows);
        self.scroll_offset = self.scroll_offset.min(max);
        self
    }

    /// Apply an upward scroll from the tail, saturating at the oldest retained line.
    #[must_use]
    pub fn scrolled_up(mut self, lines: usize, visible_rows: usize) -> Self {
        let max = self.max_scroll_offset(visible_rows);
        self.scroll_offset = self.scroll_offset.saturating_add(lines).min(max);
        self
    }

    /// Apply a downward scroll toward the tail. Offset `0` resumes follow-tail.
    #[must_use]
    pub const fn scrolled_down(mut self, lines: usize) -> Self {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
        self
    }
}

impl SwitcherEntry {
    /// Build an entry from a chat-side session view.
    #[must_use]
    pub fn from_view(view: &ManagedSessionView) -> Self {
        Self {
            seq: view.seq,
            kind: view.kind.as_str(),
            origin: view.origin.as_str(),
            status: view.status.as_str(),
            title: view.title.clone(),
            created_at: view.created_at,
            updated_at: view.updated_at,
            token_usage_records: view.token_usage_records.clone(),
            idle_warning: false,
        }
    }

    #[must_use]
    pub fn token_usage_summary(&self) -> Option<crate::chat::session::SessionTokenUsageSummary> {
        crate::chat::session::summarize_session_token_usage(&self.token_usage_records)
    }

    /// A compact status glyph for accessibility / no-color rendering (§0.2.1 F):
    /// status is conveyed by shape, not only color. Running uses an hourglass,
    /// terminal states use check/cross marks.
    #[must_use]
    pub fn status_glyph(&self) -> &'static str {
        match self.status {
            s if s == ManagedStatus::Running.as_str() => "⏳",
            s if s == ManagedStatus::NeedsInput.as_str() => "❓",
            s if s == ManagedStatus::Completed.as_str() => "✓",
            s if s == ManagedStatus::Cancelled.as_str() => "⊘",
            _ => "✗",
        }
    }

    /// Whether this session is in a terminal (non-running) state. Used only for
    /// display de-emphasis; attaching to a terminal session is still allowed
    /// (it shows the final history tail).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        // Compare against the stable status labels rather than re-deriving the
        // enum, keeping this entry self-contained.
        matches!(
            self.status,
            s if s == ManagedStatus::Completed.as_str()
                || s == ManagedStatus::Failed.as_str()
                || s == ManagedStatus::Cancelled.as_str()
        )
    }

    /// Compact elapsed runtime label derived only from carried timestamps.
    #[must_use]
    pub fn elapsed_label(&self) -> String {
        format_elapsed_compact(elapsed_seconds_between(self.created_at, self.updated_at))
    }

    /// Whether this row is the synthetic read-only transcript viewer.
    #[must_use]
    pub fn is_transcript(&self) -> bool {
        self.kind == ManagedKind::Transcript.as_str()
    }
}

/// Directional child-session navigation used by P3 Left/Right switching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDirection {
    /// Move to the visually previous session in the strip/list order.
    Previous,
    /// Move to the visually next session in the strip/list order.
    Next,
}

/// Return the adjacent live session seq in the same visual order used by the
/// sessions strip and Ctrl+G switcher. Terminal entries are skipped so
/// directional navigation stays on live child surfaces; completed sessions
/// remain reachable through random access (`Ctrl+G` / `/attach N`).
#[must_use]
pub fn adjacent_session_seq(entries: &[SwitcherEntry], current_seq: u64, direction: SessionDirection) -> Option<u64> {
    let live: Vec<&SwitcherEntry> = entries
        .iter()
        .filter(|entry| !entry.is_terminal() && !entry.is_transcript())
        .collect();
    if live.len() < 2 {
        return None;
    }
    let current_idx = live.iter().position(|entry| entry.seq == current_seq)?;
    let target_idx = match direction {
        SessionDirection::Previous => current_idx
            .checked_sub(1)
            .unwrap_or_else(|| live.len().saturating_sub(1)),
        SessionDirection::Next => {
            let next = current_idx.saturating_add(1);
            if next >= live.len() { 0 } else { next }
        }
    };
    live.get(target_idx).map(|entry| entry.seq)
}

/// The Ctrl+G session switcher overlay state. `None` (in `TuiState`) means the
/// switcher is closed.
#[derive(Debug, Clone, PartialEq)]
pub struct SwitcherState {
    /// Snapshot of child TUI sessions at open time (refreshed from the cached
    /// 1s poll; the actual attach re-resolves the seq, so display staleness
    /// never causes a wrong attach).
    pub entries: Vec<SwitcherEntry>,
    /// Currently highlighted row index into `entries`. Always clamped to a
    /// valid index when `entries` is non-empty.
    pub selected: usize,
}

impl SwitcherState {
    /// Open the switcher over a session snapshot, selecting the first row.
    #[must_use]
    pub const fn new(entries: Vec<SwitcherEntry>) -> Self {
        Self { entries, selected: 0 }
    }

    /// Whether the switcher has no sessions to show.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of sessions listed.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Move the highlight up one row (saturating at the top). No-op when empty.
    pub const fn select_prev(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the highlight down one row (clamped to the last row). No-op when
    /// empty.
    pub fn select_next(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len().saturating_sub(1);
        self.selected = (self.selected + 1).min(last);
    }

    /// The display sequence `#N` of the currently highlighted session, if any.
    #[must_use]
    pub fn selected_seq(&self) -> Option<u64> {
        self.entries.get(self.selected).map(|e| e.seq)
    }

    /// The currently highlighted row, if any.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&SwitcherEntry> {
        self.entries.get(self.selected)
    }
}

/// Build switcher entries from a session snapshot (preserving order).
#[must_use]
pub fn switcher_entries(views: &[ManagedSessionView]) -> Vec<SwitcherEntry> {
    views.iter().map(SwitcherEntry::from_view).collect()
}

/// The decision produced by [`resolve_esc`] — what the Esc key should do given
/// the current input/focus/switcher context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscAction {
    /// A switcher overlay is open → close it (highest priority).
    CloseSwitcher,
    /// Input buffer is non-empty → clear it (preserves existing muscle memory).
    ClearInput,
    /// A main turn is generating → interrupt it before local input/focus cleanup.
    CancelGenerating,
    /// Input is empty and a session is focused → detach back to main.
    RequestDetach,
    /// Input is empty and the read-only transcript viewer is focused → close it.
    CloseTranscript,
    /// Input is empty and a foreground tool approval is focused → deny it.
    DenyApproval,
    /// Input is empty and the read-only diff viewer is focused → close it.
    CloseDiff,
    /// Input is empty and focus is main → the existing cancel semantics.
    Cancel,
}

/// Pure, context-dependent resolution of the Esc key (plan §v1.1 P0-8).
///
/// Priority order, deliberately layered so modal/overlay cleanup stays above
/// turn interruption. Callers handle slash menus, strip selection, and approval
/// prompts before falling through to this resolver.
/// 1. Switcher open → close the switcher.
/// 2. Approval focus → deny the pending tool.
/// 3. Generating → interrupt the active turn.
/// 4. Input non-empty → clear input (muscle memory preserved).
/// 5. Input empty + session focused → detach.
/// 6. Input empty + transcript focused → close transcript.
/// 7. Input empty + diff focused → close diff.
/// 8. Input empty + main focus → cancel (unchanged legacy behaviour).
#[must_use]
pub const fn resolve_esc(input_empty: bool, focus: FocusTarget, switcher_open: bool, generating: bool) -> EscAction {
    if switcher_open {
        return EscAction::CloseSwitcher;
    }
    if matches!(focus, FocusTarget::Approval) {
        return EscAction::DenyApproval;
    }
    if generating {
        return EscAction::CancelGenerating;
    }
    if !input_empty {
        return EscAction::ClearInput;
    }
    match focus {
        FocusTarget::Session { .. } => return EscAction::RequestDetach,
        FocusTarget::Transcript => return EscAction::CloseTranscript,
        FocusTarget::Approval => {}
        FocusTarget::Diff => return EscAction::CloseDiff,
        FocusTarget::Main => {}
    }
    EscAction::Cancel
}

/// Pure model of the input-routing focus transition used by the v1.1b key
/// thread (P0 attach/detach race fix).
///
/// The synchronous key thread, on switcher-Enter (`/attach N`) or empty-Esc
/// (`/detach`), must point the prompt indicator, its own Esc judgment, and the
/// next submittable input at the same target *before* the synthetic command is
/// enqueued — otherwise the FIFO `input_tx` routes a just-typed line to the new
/// session while the user still sees the old prompt. [`optimistic_focus`] is the
/// pure decision the key thread applies; [`rollback_focus`] is what the async
/// main loop restores if an attach ultimately fails.
///
/// `Attach(seq)` optimistically focuses that session; `Detach` returns to main.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingIntent {
    /// Switcher Enter / typed `/attach N` → focus session `#seq`.
    Attach { seq: u64 },
    /// Empty Esc / typed `/detach` → return routing to the main chat agent.
    Detach,
}

/// The focus the key thread optimistically applies for a routing intent.
///
/// This is the single value written to all three authorities at once
/// (`mirror.focus`, the reducer snapshot via `SessionFocusChanged`, and — by
/// virtue of being enqueued ahead of any later input on the same FIFO — the
/// effective routing target), keeping perception and routing consistent.
#[must_use]
pub const fn optimistic_focus(intent: RoutingIntent) -> FocusTarget {
    match intent {
        RoutingIntent::Attach { seq } => FocusTarget::Session { seq },
        RoutingIntent::Detach => FocusTarget::Main,
    }
}

/// The focus the main loop restores when an optimistic attach fails, given the
/// still-authoritative currently-followed sequence (`None` ⇒ main).
///
/// On attach failure `attached_follow` is unchanged, so the prompt must snap
/// back to whatever was actually focused before the optimistic set — never the
/// failed target.
#[must_use]
pub const fn rollback_focus(current_follow_seq: Option<u64>) -> FocusTarget {
    match current_follow_seq {
        Some(seq) => FocusTarget::Session { seq },
        None => FocusTarget::Main,
    }
}

#[cfg(test)]
mod tests {
    use super::super::id::SessionId;
    use super::super::model::ManagedKind;
    use super::*;
    use chrono::Utc;

    fn view(seq: u64, status: ManagedStatus, title: &str) -> ManagedSessionView {
        ManagedSessionView {
            id: SessionId::from_run_id(&format!("r{seq}")),
            seq,
            kind: ManagedKind::Agent,
            origin: super::super::model::SessionOrigin::User,
            title: title.to_string(),
            status,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_usage_records: Vec::new(),
        }
    }

    #[test]
    fn focus_target_helpers() {
        assert!(!FocusTarget::Main.is_session());
        assert!(!FocusTarget::Main.is_child_view());
        assert_eq!(FocusTarget::Main.session_seq(), None);
        let f = FocusTarget::Session { seq: 3 };
        assert!(f.is_session());
        assert!(f.is_child_view());
        assert_eq!(f.session_seq(), Some(3));
        assert!(!FocusTarget::Transcript.is_session());
        assert!(FocusTarget::Transcript.is_child_view());
        assert_eq!(FocusTarget::Transcript.session_seq(), None);
        assert!(!FocusTarget::Diff.is_session());
        assert!(FocusTarget::Diff.is_child_view());
        assert_eq!(FocusTarget::Diff.session_seq(), None);
    }

    #[test]
    fn switcher_navigation_clamps() {
        let mut sw = SwitcherState::new(switcher_entries(&[
            view(1, ManagedStatus::Running, "a"),
            view(2, ManagedStatus::Completed, "b"),
        ]));
        assert_eq!(sw.selected, 0);
        // Up at the top is a no-op.
        sw.select_prev();
        assert_eq!(sw.selected, 0);
        sw.select_next();
        assert_eq!(sw.selected, 1);
        // Down at the bottom is clamped.
        sw.select_next();
        assert_eq!(sw.selected, 1);
        assert_eq!(sw.selected_seq(), Some(2));
        sw.select_prev();
        assert_eq!(sw.selected_seq(), Some(1));
    }

    #[test]
    fn switcher_empty_navigation_is_noop() {
        let mut sw = SwitcherState::new(Vec::new());
        assert!(sw.is_empty());
        sw.select_next();
        sw.select_prev();
        assert_eq!(sw.selected, 0);
        assert_eq!(sw.selected_seq(), None);
    }

    #[test]
    fn switcher_entry_terminal_flag() {
        let e = SwitcherEntry::from_view(&view(1, ManagedStatus::Running, "x"));
        assert!(!e.is_terminal());
        for st in [
            ManagedStatus::Completed,
            ManagedStatus::Failed,
            ManagedStatus::Cancelled,
        ] {
            assert!(SwitcherEntry::from_view(&view(1, st, "x")).is_terminal());
        }
    }

    #[test]
    fn adjacent_session_seq_wraps_in_visual_order() {
        let entries = switcher_entries(&[
            view(1, ManagedStatus::Running, "left"),
            view(2, ManagedStatus::Running, "middle"),
            view(3, ManagedStatus::Running, "right"),
        ]);

        assert_eq!(
            adjacent_session_seq(&entries, 1, SessionDirection::Next),
            Some(2),
            "Right moves to the session visually to the right"
        );
        assert_eq!(
            adjacent_session_seq(&entries, 3, SessionDirection::Next),
            Some(1),
            "Right wraps at the end"
        );
        assert_eq!(
            adjacent_session_seq(&entries, 1, SessionDirection::Previous),
            Some(3),
            "Left wraps to the visual tail"
        );
        assert_eq!(
            adjacent_session_seq(&entries, 3, SessionDirection::Previous),
            Some(2),
            "Left moves to the session visually to the left"
        );
    }

    #[test]
    fn adjacent_session_seq_skips_terminal_entries() {
        let entries = switcher_entries(&[
            view(1, ManagedStatus::Running, "left"),
            view(2, ManagedStatus::Completed, "done"),
            view(3, ManagedStatus::Failed, "failed"),
            view(4, ManagedStatus::Cancelled, "cancelled"),
            view(5, ManagedStatus::Running, "right"),
        ]);

        assert_eq!(adjacent_session_seq(&entries, 1, SessionDirection::Next), Some(5));
        assert_eq!(adjacent_session_seq(&entries, 5, SessionDirection::Previous), Some(1));
        assert_eq!(
            adjacent_session_seq(&entries, 3, SessionDirection::Next),
            None,
            "Failed terminal rows are not current switch targets"
        );
        assert_eq!(
            adjacent_session_seq(&entries, 4, SessionDirection::Previous),
            None,
            "Cancelled terminal rows are not current switch targets"
        );
    }

    #[test]
    fn adjacent_session_seq_skips_transcript_entry() {
        let mut entries = vec![SwitcherEntry {
            seq: 0,
            kind: ManagedKind::Transcript.as_str(),
            origin: "user",
            status: "ready",
            title: "conversation transcript".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        }];
        entries.extend(switcher_entries(&[
            view(1, ManagedStatus::Running, "left"),
            view(2, ManagedStatus::Running, "right"),
        ]));

        assert_eq!(adjacent_session_seq(&entries, 1, SessionDirection::Previous), Some(2));
        assert_eq!(adjacent_session_seq(&entries, 2, SessionDirection::Next), Some(1));
        assert_eq!(
            adjacent_session_seq(&entries, 0, SessionDirection::Next),
            None,
            "transcript seq must not be a switchable real session"
        );
    }

    #[test]
    fn adjacent_session_seq_none_for_empty_one_or_unknown_current() {
        assert_eq!(adjacent_session_seq(&[], 1, SessionDirection::Next), None);

        let one = switcher_entries(&[view(1, ManagedStatus::Running, "only")]);
        assert_eq!(adjacent_session_seq(&one, 1, SessionDirection::Next), None);

        let terminal_plus_one = switcher_entries(&[
            view(1, ManagedStatus::Completed, "done"),
            view(2, ManagedStatus::Running, "only-live"),
        ]);
        assert_eq!(
            adjacent_session_seq(&terminal_plus_one, 2, SessionDirection::Previous),
            None
        );

        let entries = switcher_entries(&[
            view(1, ManagedStatus::Running, "left"),
            view(2, ManagedStatus::Running, "right"),
        ]);
        assert_eq!(adjacent_session_seq(&entries, 99, SessionDirection::Next), None);
    }

    #[test]
    fn active_session_view_scroll_offset_saturates_and_resumes_follow() {
        let view = ActiveSessionView {
            seq: 1,
            kind: "agent".to_string(),
            title: "task".to_string(),
            lines: (0..12).map(|i| format!("line {i}")).collect(),
            truncated: false,
            scroll_offset: 0,
        };

        assert_eq!(view.max_scroll_offset(5), 7);
        let view = view.scrolled_up(3, 5);
        assert_eq!(view.scroll_offset, 3);
        let view = view.scrolled_up(99, 5);
        assert_eq!(view.scroll_offset, 7, "up scroll clamps at oldest retained line");
        let view = view.scrolled_down(4);
        assert_eq!(view.scroll_offset, 3);
        let view = view.scrolled_down(99);
        assert_eq!(view.scroll_offset, 0, "down scroll saturates back to follow-tail");
    }

    #[test]
    fn active_session_view_clamps_when_visible_rows_exceed_lines() {
        let view = ActiveSessionView {
            seq: 2,
            kind: "shell".to_string(),
            title: "cmd".to_string(),
            lines: vec!["only".to_string()],
            truncated: true,
            scroll_offset: 99,
        };
        assert_eq!(view.clamped_for_height(10).scroll_offset, 0);
    }

    /// v5: build a typed view to verify the switcher lists all three kinds and
    /// carries the kind + origin labels through to the entry.
    fn typed_view(
        seq: u64,
        kind: super::super::model::ManagedKind,
        origin: super::super::model::SessionOrigin,
    ) -> ManagedSessionView {
        ManagedSessionView {
            id: SessionId::from_run_id(&format!("r{seq}")),
            seq,
            kind,
            origin,
            title: format!("t{seq}"),
            status: ManagedStatus::Running,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_usage_records: Vec::new(),
        }
    }

    #[test]
    fn switcher_lists_all_three_kinds_with_labels() {
        use super::super::model::{ManagedKind, SessionOrigin};
        let views = vec![
            typed_view(1, ManagedKind::Agent, SessionOrigin::Model),
            typed_view(2, ManagedKind::Shell, SessionOrigin::User),
            typed_view(3, ManagedKind::Pty, SessionOrigin::User),
        ];
        let entries = switcher_entries(&views);
        assert_eq!(entries.len(), 3, "all three kinds present in the switcher");
        let kinds: Vec<(&str, &str)> = entries.iter().map(|e| (e.kind, e.origin)).collect();
        assert_eq!(
            kinds,
            vec![("agent", "model"), ("shell", "user"), ("pty", "user")],
            "kind + origin labels thread through the switcher"
        );
        let seqs: Vec<u64> = entries.iter().map(|entry| entry.seq).collect();
        assert_eq!(
            seqs,
            vec![1, 2, 3],
            "switcher entries must preserve the visual left-to-right strip order"
        );
    }

    #[test]
    fn switcher_entry_status_glyph_is_shape_coded() {
        // Accessibility: distinct shapes per status (not only colour).
        let running = SwitcherEntry::from_view(&view(1, ManagedStatus::Running, "x"));
        let completed = SwitcherEntry::from_view(&view(2, ManagedStatus::Completed, "x"));
        let failed = SwitcherEntry::from_view(&view(3, ManagedStatus::Failed, "x"));
        let cancelled = SwitcherEntry::from_view(&view(4, ManagedStatus::Cancelled, "x"));
        // All four are distinct glyphs.
        let glyphs = [
            running.status_glyph(),
            completed.status_glyph(),
            failed.status_glyph(),
            cancelled.status_glyph(),
        ];
        for (i, a) in glyphs.iter().enumerate() {
            for (j, b) in glyphs.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "status glyphs must be distinct ({i} vs {j})");
                }
            }
        }
    }

    #[test]
    fn resolve_esc_switcher_open_takes_priority() {
        // Even with non-empty input and session focus, an open switcher closes first.
        assert_eq!(
            resolve_esc(false, FocusTarget::Session { seq: 1 }, true, false),
            EscAction::CloseSwitcher
        );
        assert_eq!(
            resolve_esc(true, FocusTarget::Main, true, false),
            EscAction::CloseSwitcher
        );
    }

    #[test]
    fn resolve_esc_generating_interrupts_before_input_or_focus_cleanup() {
        assert_eq!(
            resolve_esc(false, FocusTarget::Session { seq: 1 }, false, true),
            EscAction::CancelGenerating
        );
        assert_eq!(
            resolve_esc(true, FocusTarget::Diff, false, true),
            EscAction::CancelGenerating
        );
    }

    #[test]
    fn resolve_esc_approval_focus_takes_priority_over_generating() {
        assert_eq!(
            resolve_esc(true, FocusTarget::Approval, false, true),
            EscAction::DenyApproval
        );
        assert_eq!(
            resolve_esc(false, FocusTarget::Approval, false, true),
            EscAction::DenyApproval
        );
    }

    #[test]
    fn resolve_esc_nonempty_clears_input() {
        // Muscle memory: non-empty input always clears first (when no switcher).
        assert_eq!(
            resolve_esc(false, FocusTarget::Main, false, false),
            EscAction::ClearInput
        );
        assert_eq!(
            resolve_esc(false, FocusTarget::Session { seq: 2 }, false, false),
            EscAction::ClearInput
        );
        assert_eq!(
            resolve_esc(false, FocusTarget::Transcript, false, false),
            EscAction::ClearInput
        );
        assert_eq!(
            resolve_esc(false, FocusTarget::Diff, false, false),
            EscAction::ClearInput
        );
    }

    #[test]
    fn resolve_esc_empty_session_detaches() {
        assert_eq!(
            resolve_esc(true, FocusTarget::Session { seq: 5 }, false, false),
            EscAction::RequestDetach
        );
    }

    #[test]
    fn resolve_esc_empty_transcript_closes_transcript() {
        assert_eq!(
            resolve_esc(true, FocusTarget::Transcript, false, false),
            EscAction::CloseTranscript
        );
    }

    #[test]
    fn resolve_esc_empty_diff_closes_diff() {
        assert_eq!(resolve_esc(true, FocusTarget::Diff, false, false), EscAction::CloseDiff);
    }

    #[test]
    fn resolve_esc_empty_main_cancels() {
        assert_eq!(resolve_esc(true, FocusTarget::Main, false, false), EscAction::Cancel);
    }

    // ── v1.1b P0: attach/detach input-routing race ──────────────────────────
    //
    // These cover the invariant Codex flagged: the prompt indicator, the key
    // thread's Esc judgment, and the FIFO routing target must agree the instant
    // a `/attach` / `/detach` is enqueued. The key thread enqueues
    // `optimistic_focus(intent)` on all three authorities before sending the
    // synthetic command, so a line typed immediately afterwards is *perceived*
    // to go exactly where FIFO ordering will actually route it.

    #[test]
    fn optimistic_attach_focuses_target_session() {
        assert_eq!(
            optimistic_focus(RoutingIntent::Attach { seq: 7 }),
            FocusTarget::Session { seq: 7 }
        );
    }

    #[test]
    fn optimistic_detach_focuses_main() {
        assert_eq!(optimistic_focus(RoutingIntent::Detach), FocusTarget::Main);
    }

    #[test]
    fn ctrl_g_enter_then_immediate_input_sees_attached_prompt() {
        // Models: Ctrl+G Enter selects #5, then the user types + Enter before the
        // async main loop has processed the synthetic `/attach`. Because the key
        // thread applies the optimistic focus first, the Esc judgment for that
        // next (empty) input already detaches from #5 — i.e. perception tracks
        // the new target, not the stale Main.
        let focus = optimistic_focus(RoutingIntent::Attach { seq: 5 });
        assert_eq!(focus, FocusTarget::Session { seq: 5 });
        // Next submittable input perceives session focus, matching FIFO routing.
        assert_eq!(resolve_esc(true, focus, false, false), EscAction::RequestDetach);
    }

    #[test]
    fn esc_detach_then_immediate_input_sees_main_prompt() {
        // Symmetric: empty-Esc detach optimistically returns to Main, so the next
        // input is perceived (and routed) as a main-chat turn, not a stale steer.
        let focus = optimistic_focus(RoutingIntent::Detach);
        assert_eq!(focus, FocusTarget::Main);
        assert_eq!(resolve_esc(true, focus, false, false), EscAction::Cancel);
    }

    #[test]
    fn rollback_on_attach_failure_restores_previous_target() {
        // No prior follow → a failed attach snaps the prompt back to Main, never
        // the failed target.
        assert_eq!(rollback_focus(None), FocusTarget::Main);
        // Already following #2 and an attach to a now-gone seq fails → restore #2.
        assert_eq!(rollback_focus(Some(2)), FocusTarget::Session { seq: 2 });
    }

    #[test]
    fn rollback_is_inverse_of_optimistic_when_attach_fails_from_main() {
        // Start at Main, optimistically attach #9, attach fails → rollback to the
        // unchanged follow (None ⇒ Main): perception ends consistent with routing.
        let optimistic = optimistic_focus(RoutingIntent::Attach { seq: 9 });
        assert_eq!(optimistic, FocusTarget::Session { seq: 9 });
        let current_follow_seq: Option<u64> = None; // attach never bound it
        assert_eq!(rollback_focus(current_follow_seq), FocusTarget::Main);
    }
}
