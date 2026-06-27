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

use super::model::{ManagedSessionView, ManagedStatus};

/// Where plain text + Enter is currently routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusTarget {
    /// Default: input goes to the main chat agent loop.
    #[default]
    Main,
    /// Input is routed as a *steer* to the attached child TUI session `#seq`.
    Session { seq: u64 },
}

impl FocusTarget {
    /// Whether a child TUI session currently has input focus.
    #[must_use]
    pub const fn is_session(self) -> bool {
        matches!(self, Self::Session { .. })
    }

    /// The focused session's display sequence `#N`, if any.
    #[must_use]
    pub const fn session_seq(self) -> Option<u64> {
        match self {
            Self::Main => None,
            Self::Session { seq } => Some(seq),
        }
    }
}

/// One row in the Ctrl+G session switcher. A plain display snapshot (no async,
/// no registry handle) so the synchronous key thread can render and navigate it.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        }
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
}

/// The Ctrl+G session switcher overlay state. `None` (in `TuiState`) means the
/// switcher is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Input is empty and a session is focused → detach back to main.
    RequestDetach,
    /// Input is empty and focus is main → the existing cancel semantics.
    Cancel,
}

/// Pure, context-dependent resolution of the Esc key (plan §v1.1 P0-8).
///
/// Priority order, deliberately layered so the established "non-empty input
/// clears" behaviour is never weakened:
/// 1. Switcher open → close the switcher.
/// 2. Input non-empty → clear input (muscle memory preserved).
/// 3. Input empty + session focused → detach.
/// 4. Input empty + main focus → cancel (unchanged legacy behaviour).
#[must_use]
pub const fn resolve_esc(input_empty: bool, focus: FocusTarget, switcher_open: bool) -> EscAction {
    if switcher_open {
        return EscAction::CloseSwitcher;
    }
    if !input_empty {
        return EscAction::ClearInput;
    }
    if focus.is_session() {
        return EscAction::RequestDetach;
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
        }
    }

    #[test]
    fn focus_target_helpers() {
        assert!(!FocusTarget::Main.is_session());
        assert_eq!(FocusTarget::Main.session_seq(), None);
        let f = FocusTarget::Session { seq: 3 };
        assert!(f.is_session());
        assert_eq!(f.session_seq(), Some(3));
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
            resolve_esc(false, FocusTarget::Session { seq: 1 }, true),
            EscAction::CloseSwitcher
        );
        assert_eq!(resolve_esc(true, FocusTarget::Main, true), EscAction::CloseSwitcher);
    }

    #[test]
    fn resolve_esc_nonempty_clears_input() {
        // Muscle memory: non-empty input always clears first (when no switcher).
        assert_eq!(resolve_esc(false, FocusTarget::Main, false), EscAction::ClearInput);
        assert_eq!(
            resolve_esc(false, FocusTarget::Session { seq: 2 }, false),
            EscAction::ClearInput
        );
    }

    #[test]
    fn resolve_esc_empty_session_detaches() {
        assert_eq!(
            resolve_esc(true, FocusTarget::Session { seq: 5 }, false),
            EscAction::RequestDetach
        );
    }

    #[test]
    fn resolve_esc_empty_main_cancels() {
        assert_eq!(resolve_esc(true, FocusTarget::Main, false), EscAction::Cancel);
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
        assert_eq!(resolve_esc(true, focus, false), EscAction::RequestDetach);
    }

    #[test]
    fn esc_detach_then_immediate_input_sees_main_prompt() {
        // Symmetric: empty-Esc detach optimistically returns to Main, so the next
        // input is perceived (and routed) as a main-chat turn, not a stale steer.
        let focus = optimistic_focus(RoutingIntent::Detach);
        assert_eq!(focus, FocusTarget::Main);
        assert_eq!(resolve_esc(true, focus, false), EscAction::Cancel);
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
