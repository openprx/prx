//! Interactive PTY shell sessions (v3a) — a full-screen terminal handoff.
//!
//! Unlike the v2 [`super::shell::ShellSession`] (a one-shot, non-interactive
//! command streamed line-by-line through the event bridge), a
//! [`PtyShellSession`] allocates a real pseudo-terminal so programs that *need*
//! a TTY work correctly: `sh`/`bash` interactive shells, `python`/`node` REPLs,
//! `vim`, `top`, `npm run dev`, etc. The trade-off is that an interactive PTY is
//! **not** a soft view rendered inside the chat TUI — it is a genuine
//! full-screen terminal handoff: while attached, the chat ratatui render loop is
//! suspended and the real terminal (raw stdin + stdout) is wired straight to the
//! PTY. On detach (or when the PTY child exits) the chat TUI is restored.
//!
//! # Why this module is the highest-risk part of the session runtime
//!
//! Two parties want exclusive control of the same physical terminal:
//!
//! - the chat **render loop** (a `spawn_blocking` thread that owns
//!   `ratatui::Terminal` + `stdout`, reads keys via `crossterm::event`); and
//! - the **PTY passthrough** (raw `stdin` bytes → PTY writer, PTY reader →
//!   raw `stdout`).
//!
//! They must never both hold the terminal at once. The handoff is coordinated by
//! [`HandoffControl`] (a pause flag the render loop polls + a condvar the main
//! loop waits on for a deterministic "render loop has parked" acknowledgement),
//! and restoration is guaranteed by the RAII [`PtyHandoffGuard`]: its `Drop`
//! resumes the render loop and forces a full redraw **on every exit path**
//! (normal detach, PTY crash, an `?` early-return, or a panic unwinding through
//! the handoff). We would rather ship a smaller feature than one that can leave
//! the user's terminal wedged.
//!
//! # Process-group semantics
//!
//! On Unix `portable-pty` makes the spawned child a session leader (`setsid`),
//! so its process-group id equals its pid. We reuse
//! [`super::shell::kill_process_group`] (SIGTERM → grace → SIGKILL via `killpg`)
//! to terminate the **whole** group on `/kill` or chat exit, so a shell that
//! backgrounds children (`sleep 100 &`) leaves no orphans. On non-Unix targets
//! `portable-pty`'s ConPTY backend has different semantics; we fall back to the
//! child killer and document the limitation.

use super::id::SessionId;
use anyhow::{Result, anyhow};
use parking_lot::{Condvar, Mutex};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Control byte that detaches from an attached interactive PTY (`Ctrl-]`,
/// `0x1d`). This is the classic telnet/ssh escape: it is rarely meaningful to a
/// shell, and crucially it is **not** `Ctrl-C` (`0x03`) or `Ctrl-D` (`0x04`),
/// both of which must pass through to the PTY child unchanged so the user can
/// interrupt a foreground process (Ctrl-C) or signal EOF (Ctrl-D) without
/// detaching.
pub const DETACH_BYTE: u8 = 0x1d;

/// `Ctrl-C` — forwarded to the PTY as an interrupt, never treated as detach.
pub const CTRL_C: u8 = 0x03;
/// `Ctrl-D` — forwarded to the PTY as EOF, never treated as detach.
pub const CTRL_D: u8 = 0x04;

/// Coordination handle shared between the chat render loop and the PTY handoff.
///
/// The render loop (a synchronous `spawn_blocking` thread) checks
/// [`HandoffControl::is_paused`] at the top of each iteration; while paused it
/// skips all terminal I/O (no `crossterm` poll/read, no draw) and parks briefly,
/// yielding `stdin`/`stdout` to the PTY passthrough. To make the handoff
/// deterministic (no race where the render loop is mid-`read` and steals the
/// first PTY keystroke), the render loop calls [`HandoffControl::ack_paused`]
/// once it has actually entered the paused branch, and the main loop blocks in
/// [`HandoffControl::pause_and_wait`] until that acknowledgement arrives.
///
/// The render loop also reads [`HandoffControl::take_force_redraw`] on resume: a
/// `true` there means "the PTY scribbled all over the screen — clear and fully
/// repaint".
///
/// All primitives are synchronous (`parking_lot` + atomics). The render loop is
/// not async, and the main loop only ever touches these from a short
/// non-`.await` critical section, so no `tokio` sync types are needed and the
/// "never hold a lock across `.await`" iron law is upheld.
#[derive(Debug)]
pub struct HandoffControl {
    /// True while the render loop must stay parked (terminal handed to the PTY).
    paused: AtomicBool,
    /// Set by the guard on resume; the render loop clears it and, when it was
    /// set, forces a `terminal.clear()` + full redraw to wipe PTY residue.
    force_redraw: AtomicBool,
    /// `(render_loop_has_parked)` flag + condvar for the deterministic ack.
    parked: Mutex<bool>,
    /// Notified by the render loop when it transitions parked → true and by the
    /// guard when it clears `paused` (so a still-running render loop, if it ever
    /// waited, would wake — defensive).
    parked_cv: Condvar,
}

impl Default for HandoffControl {
    fn default() -> Self {
        Self::new()
    }
}

impl HandoffControl {
    /// A fresh control in the resumed (not-paused) state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            force_redraw: AtomicBool::new(false),
            parked: Mutex::new(false),
            parked_cv: Condvar::new(),
        }
    }

    /// Whether the render loop should currently stay parked.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    /// Called by the render loop once it has entered (or stayed in) the paused
    /// branch this iteration, publishing "I am parked and not touching the
    /// terminal" so [`pause_and_wait`](Self::pause_and_wait) can return.
    pub fn ack_paused(&self) {
        let mut parked = self.parked.lock();
        if !*parked {
            *parked = true;
            self.parked_cv.notify_all();
        }
    }

    /// Take (and clear) the "force a full redraw on resume" flag. Called by the
    /// render loop right after it observes `!is_paused()`; a returned `true`
    /// means the PTY left residue on screen and the loop must
    /// `terminal.clear()` + repaint the whole frame.
    pub fn take_force_redraw(&self) -> bool {
        self.force_redraw.swap(false, Ordering::AcqRel)
    }

    /// Request the pause and **block until the render loop acknowledges** it has
    /// parked, so the caller can safely take over `stdin`/`stdout`.
    ///
    /// `timeout` bounds the wait: if the render loop does not acknowledge in
    /// time (e.g. it is wedged inside a slow draw, or the TUI was
    /// never started) we proceed anyway after the timeout — handoff is
    /// best-effort, never a deadlock. Returns `true` if the ack was observed,
    /// `false` if we proceeded on timeout.
    ///
    /// Synchronous (`parking_lot` condvar); the caller invokes it from a
    /// `spawn_blocking` context, never holding the lock across an `.await`.
    pub fn pause_and_wait(&self, timeout: std::time::Duration) -> bool {
        // Reset the ack flag, then publish the pause request.
        {
            let mut parked = self.parked.lock();
            *parked = false;
        }
        self.paused.store(true, Ordering::Release);
        // Wait for the render loop to confirm it has parked.
        let deadline = std::time::Instant::now() + timeout;
        let mut parked = self.parked.lock();
        while !*parked {
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let remaining = deadline - now;
            // `wait_for` releases the lock while waiting and re-acquires on wake.
            if self.parked_cv.wait_for(&mut parked, remaining).timed_out() && !*parked {
                return false;
            }
        }
        true
    }

    /// Resume the render loop and request a full redraw to wipe PTY residue.
    /// Idempotent; called by [`PtyHandoffGuard::drop`].
    pub fn resume_with_redraw(&self) {
        self.force_redraw.store(true, Ordering::Release);
        self.paused.store(false, Ordering::Release);
        // Clear the ack so the next handoff starts from a known state and wake
        // any (defensive) waiter.
        let mut parked = self.parked.lock();
        *parked = false;
        self.parked_cv.notify_all();
    }
}

/// RAII guard that owns the *restoration* half of a terminal handoff.
///
/// Constructing a guard records that the terminal has been handed to a PTY;
/// dropping it — on **any** path: normal detach, PTY child crash, `?`
/// early-return, or a panic unwinding through the handoff — restores the chat
/// TUI by:
///
/// 1. resuming the render loop ([`HandoffControl::resume_with_redraw`]), which
///    re-takes ownership of `Terminal`/`stdout` and forces a
///    `terminal.clear()` + full redraw on its next iteration; and
/// 2. defensively re-asserting raw mode + bracketed paste, since the PTY child
///    may have changed the terminal's `termios` (e.g. a REPL that disabled echo
///    and exited abnormally). The chat `TerminalGuard` normally still owns these
///    process-wide, but re-asserting here is cheap and closes the gap where a
///    misbehaving child left the terminal cooked.
///
/// The guard performs **no** terminal writes that could conflict with the
/// resumed render loop beyond the `crossterm` mode calls (which are idempotent
/// kernel/escape operations); it never touches `ratatui::Terminal` directly
/// (the render loop owns it), avoiding a cross-thread aliasing of `stdout`.
#[must_use = "dropping the guard is what restores the terminal; bind it for the handoff's lifetime"]
pub struct PtyHandoffGuard {
    control: Arc<HandoffControl>,
    keyboard_enhancement_active: bool,
    mouse_capture_active: bool,
    /// Best-effort redraw nudge so the render loop repaints immediately on
    /// resume rather than waiting out its idle poll. `None` when the chat has no
    /// TUI render loop (fallback path); restoration still works via the flag.
    redraw_nudge: Option<Box<dyn Fn() + Send>>,
}

impl PtyHandoffGuard {
    /// Begin a terminal handoff: pause the render loop and wait for it to park,
    /// then return a guard whose `Drop` restores the chat TUI.
    ///
    /// Returns `None` if the render loop does **not** acknowledge the pause
    /// within [`PAUSE_ACK_TIMEOUT`]: in that case we cannot prove the render loop
    /// has stopped touching the terminal, so continuing the handoff would let two
    /// threads write to `stdout`/read `stdin` concurrently and corrupt the
    /// screen. We therefore **abort the attach**: the pause flag is cleared (the
    /// render loop, if alive, resumes cleanly) and the caller must not proceed.
    /// We would rather refuse `/pty attach` than wedge or scramble the terminal.
    ///
    /// `redraw_nudge`, if supplied, is invoked on `Drop` to wake the render loop
    /// immediately (e.g. a `move || { let _ = redraw_tx.try_send(()); }`).
    pub fn acquire(
        control: Arc<HandoffControl>,
        redraw_nudge: Option<Box<dyn Fn() + Send>>,
        keyboard_enhancement_active: bool,
        mouse_capture_active: bool,
    ) -> Option<Self> {
        // Block (bounded) until the render loop confirms it has parked.
        if control.pause_and_wait(PAUSE_ACK_TIMEOUT) {
            let mut out = std::io::stdout();
            write_chat_alt_screen_leave_for_handoff(&mut out, keyboard_enhancement_active);
            return Some(Self {
                control,
                keyboard_enhancement_active,
                mouse_capture_active,
                redraw_nudge,
            });
        }
        // Ack timed out: the render loop never confirmed it parked. Abandon the
        // handoff and undo the pause so the (possibly-slow-but-alive) render loop
        // resumes. No terminal control sequences are written here — the render
        // loop still owns the terminal.
        control.resume_with_redraw();
        None
    }
}

impl Drop for PtyHandoffGuard {
    fn drop(&mut self) {
        // ORDER MATTERS. While we restore the host terminal's modes the render
        // loop must still be parked, otherwise it could `draw()` (writing its
        // own control sequences to `stdout`) concurrently with the mode-reset
        // escape sequences / `enable_raw_mode` / `EnableBracketedPaste` below, or
        // step into crossterm `poll`/`read` before raw mode is re-asserted. So:
        //
        //   1a. reset the host terminal modes the PTY child clobbered
        //   1b. re-assert chat's raw mode + bracketed paste
        //   2.  arm force_redraw
        //   3.  unpause                  (render loop may now draw)
        //   4.  nudge
        //
        // After step 3 we write NO further stdout control sequences.

        // 1a. A PTY child (vim/htop/less/…) may have switched the terminal into
        //     modes the chat TUI never uses — mouse tracking, the application
        //     cursor/keypad modes, a hidden cursor, the alternate screen buffer —
        //     and on a normal `Ctrl-]` detach the child is NOT killed, so it never
        //     emits the corresponding "off" sequences. Left as-is, the chat would
        //     resume with mouse events escaping as garbage, arrow/keypad keys
        //     misbehaving (DECCKM), the cursor invisible, or the screen stuck in a
        //     leftover alt-buffer. Write a host-mode reset to pull the terminal
        //     back to chat's baseline. These sequences are idempotent and are the
        //     normal chat state anyway, so re-sending them is harmless even when no
        //     PTY child touched them (and in the no-TUI fallback path). Best-effort
        //     — we are on the restoration path with no caller to surface errors to.
        //     Done while the render loop is still parked so we are the sole writer.
        {
            let mut out = std::io::stdout();
            write_handoff_terminal_restore(&mut out, self.keyboard_enhancement_active, self.mouse_capture_active);
        }

        // 1b. Defensively re-assert the chat terminal modes the PTY child may
        //     have clobbered, while the render loop is still parked so we are the
        //     sole writer of these escape/kernel operations. Mouse capture is
        //     restored only if chat owned it before the handoff. Best-effort.
        let _ = crossterm::terminal::enable_raw_mode();
        if self.mouse_capture_active {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
        }
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);

        // 2 & 3. Arm the full redraw (to wipe PTY residue) and only THEN unpause,
        //        so the render loop's first post-resume action sees the flag and
        //        repaints cleanly. `resume_with_redraw` sets force_redraw before
        //        clearing `paused`, so the ordering holds for the render loop too.
        self.control.resume_with_redraw();

        // 4. Nudge the render loop to repaint right away (no stdout writes here;
        //    just a channel wakeup).
        if let Some(nudge) = &self.redraw_nudge {
            nudge();
        }
    }
}

/// Escape sequences that pull the host terminal back to the chat TUI's baseline
/// after a PTY child (vim/htop/less/…) may have changed its modes.
///
/// Emitted on **every** PTY detach path (normal `Ctrl-]` detach, child exit,
/// error, panic — all funnel through [`PtyHandoffGuard::drop`]), where the child
/// is typically NOT killed and so never sends its own "off" sequences. Each is
/// idempotent and matches the modes the chat TUI itself uses, so re-sending them
/// when nothing changed (or in the no-TUI fallback path) is harmless. Order is
/// irrelevant (independent modes); grouped by concern:
///
/// - `?1000l ?1002l ?1003l ?1005l ?1015l ?1006l` — disable every
///   X10/button/any-motion mouse tracking mode and the UTF-8/RXVT/SGR
///   extended-coordinate encodings, so a program that enabled mouse reporting
///   (htop, vim with `set mouse=a`) can no longer make the terminal spew mouse
///   escape bursts into chat's stdin.
/// - `?1l` — DECCKM off: cursor keys send the normal `ESC [ A` form rather than
///   the application `ESC O A` form, so arrow keys behave in the chat line editor.
/// - `ESC >` (DECKPNM) — numeric keypad mode, undoing a child's application
///   keypad (`ESC =`).
/// - `?25h` — show the cursor, undoing a full-screen program that hid it.
/// - `?1049l` then `?47l` — leave the alternate screen buffer (both the modern
///   1049 and the legacy 47 variants) so no alt-buffer residue is left behind.
const CHAT_MOUSE_CAPTURE_ENABLE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h";
const CHAT_MOUSE_CAPTURE_DISABLE: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
const CHAT_KEYBOARD_ENHANCEMENT_PUSH: &[u8] = b"\x1b[>15u";
const CHAT_KEYBOARD_ENHANCEMENT_POP: &[u8] = b"\x1b[<1u";
const HOST_MODE_RESET: &[u8] =
    b"\x1b[?1006l\x1b[?1015l\x1b[?1005l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1l\x1b>\x1b[?25h\x1b[?1049l\x1b[?47l";
const CHAT_ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";
const CHAT_ALT_SCREEN_LEAVE: &[u8] = b"\x1b[?1049l\x1b[?47l";

pub(crate) fn write_chat_alt_screen_leave_for_handoff(out: &mut dyn std::io::Write, keyboard_enhancement_active: bool) {
    let result = if keyboard_enhancement_active {
        out.write_all(CHAT_KEYBOARD_ENHANCEMENT_POP)
    } else {
        Ok(())
    }
    .and_then(|()| out.write_all(CHAT_MOUSE_CAPTURE_DISABLE))
    .and_then(|()| {
        out.write_all(CHAT_ALT_SCREEN_LEAVE)?;
        out.flush()
    });
    if let Err(e) = result {
        tracing::warn!(error = %crate::agent::diagnostics::error_class(&e), "terminal handoff: failed to leave chat alternate screen");
    }
}

pub(crate) fn write_handoff_terminal_restore(
    out: &mut dyn std::io::Write,
    keyboard_enhancement_active: bool,
    mouse_capture_active: bool,
) {
    let result = out.write_all(HOST_MODE_RESET).and_then(|()| {
        out.write_all(CHAT_ALT_SCREEN_ENTER)?;
        if mouse_capture_active {
            out.write_all(CHAT_MOUSE_CAPTURE_ENABLE)?;
        }
        if keyboard_enhancement_active {
            out.write_all(CHAT_KEYBOARD_ENHANCEMENT_PUSH)?;
        }
        out.flush()
    });
    if let Err(e) = result {
        tracing::warn!(error = %crate::agent::diagnostics::error_class(&e), "terminal handoff: failed to restore terminal modes");
    }
}

/// How long [`PtyHandoffGuard::acquire`] waits for the render loop to park
/// before proceeding anyway. Comfortably larger than the render loop's 50 ms
/// poll interval so a healthy loop always acks first, but bounded so a wedged
/// or absent loop never deadlocks the handoff.
const PAUSE_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Maximum number of *live* (running, attached-or-detached) interactive PTY
/// sessions allowed to coexist. v3b keeps detached PTYs alive (each holding a
/// drain thread + a ring buffer + master/child/writer fds), so an unbounded
/// `/pty` would leak threads and memory. Spawning past this limit is refused with
/// a hint to `/kill` an existing one. Exited (dead) sessions do not count.
pub const MAX_LIVE_PTYS: usize = 8;

/// Capacity (bytes) of a PTY's raw-output ring buffer (v3b). Detached PTY output
/// is drained into this fixed-size ring so the child never blocks on a full
/// kernel pty buffer, and so a re-attach can replay recent history to restore
/// context. Oldest bytes are dropped on overflow (like a terminal scrollback
/// cap). 256 KiB comfortably holds a few screens of scrollback for line-oriented
/// programs while bounding per-session memory.
pub const PTY_RING_CAPACITY: usize = 256 * 1024;

/// The shared output sink for a PTY's **persistent drain reader** (v3b).
///
/// A single long-lived reader thread copies PTY master output here for the whole
/// life of the session. This sink:
///
/// - **always** feeds the bytes to an in-process terminal emulator
///   ([`vt100::Parser`]) so a screen grid (cells + cursor + attributes + input
///   modes) tracking the *current* on-screen state is maintained at all times,
///   even while detached (v3b-b);
/// - **always** appends bytes to a bounded ring buffer (so detached PTYs keep
///   draining — a full kernel pty buffer would otherwise block the child — and so
///   a re-attach has raw scrollback to fall back on / diagnose with); and
/// - **when attached**, also writes the bytes straight to the real terminal
///   `stdout` (so the attached user sees live output).
///
/// Routing (the attached check + emulator feed + ring append + optional `stdout`
/// write) happens under a single `parking_lot::Mutex`, and detach flips `attached`
/// under that **same** lock. This closes the v3a invariant precisely: once detach
/// has set `attached = false`, the reader's *next* write observes it; a write
/// already in-flight completes its `stdout` write before detach can flip the flag
/// (they serialize on the mutex). So the reader is guaranteed to have stopped
/// writing `stdout` by the time detach returns — before the `PtyHandoffGuard`
/// resumes the chat render loop. Synchronous `parking_lot` only; never held across
/// `.await`.
///
/// # v3b-b: emulator-backed re-attach redraw
///
/// On re-attach we no longer replay the raw ring (which, for full-screen programs
/// like vim/htop, splices historical cursor-positioning escape sequences over the
/// real screen and scrambles it for a frame). Instead we ask the emulator for
/// [`vt100::Screen::state_formatted`] — a self-contained escape-code sequence that
/// reconstructs the *exact current screen* (every cell, the cursor position, the
/// active SGR attributes, and the input/keypad modes). Writing that to `stdout`
/// after a clear restores the screen correctly without relying on the child to
/// redraw itself. The raw ring is kept only as a diagnostic / fallback (it never
/// drives the visible re-attach picture anymore).
struct SinkInner {
    /// Whether the session is currently attached (reader should mirror to
    /// `stdout`). Guarded by the same lock as the parser/ring so flip + write
    /// serialize.
    attached: bool,
    /// In-process terminal emulator fed every PTY output byte. Its `screen()`
    /// always reflects the current on-screen state; on re-attach we render that
    /// screen's `state_formatted()` to restore the picture (v3b-b). Sized to the
    /// PTY geometry and kept in sync via [`PtySink::set_size`].
    parser: vt100::Parser,
    /// Bounded raw-byte scrollback ring. `VecDeque` for O(1) push-back /
    /// pop-front; capped at [`PTY_RING_CAPACITY`]. Diagnostic / fallback only since
    /// v3b-b; the visible re-attach redraw comes from `parser` instead.
    ring: std::collections::VecDeque<u8>,
    /// Last time the drain reader delivered bytes from the PTY.
    last_output_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl std::fmt::Debug for SinkInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (rows, cols) = self.parser.screen().size();
        f.debug_struct("SinkInner")
            .field("attached", &self.attached)
            .field("emulator_rows", &rows)
            .field("emulator_cols", &cols)
            .field("ring_len", &self.ring.len())
            .field("last_output_at", &self.last_output_at)
            .finish()
    }
}

/// Handle to a PTY's drain sink, shared between the drain reader and the session.
///
/// Cloning shares the same `Arc<Mutex<SinkInner>>`.
#[derive(Clone, Debug)]
struct PtySink(Arc<Mutex<SinkInner>>);

impl PtySink {
    /// A fresh, detached sink with an empty ring and an emulator sized to `size`.
    ///
    /// The emulator is given the PTY geometry so its screen grid matches what the
    /// child draws; `scrollback_len = 0` because the visible re-attach redraw only
    /// needs the current screen (the raw ring still keeps recent bytes for
    /// diagnostics). Geometry is kept in sync via [`PtySink::set_size`].
    fn new(size: PtySize) -> Self {
        let parser = vt100::Parser::new(size.rows.max(1), size.cols.max(1), 0);
        Self(Arc::new(Mutex::new(SinkInner {
            attached: false,
            parser,
            ring: std::collections::VecDeque::with_capacity(8192),
            last_output_at: None,
        })))
    }

    /// Mark the sink attached **and** render the emulator's current screen to
    /// `stdout` atomically under the sink lock, so the redraw can never interleave
    /// with live bytes from the drain reader (v3b-b; replaces the v3b-a raw-ring
    /// replay).
    ///
    /// Holding the sink lock across the `attached = true` flip AND the redraw write
    /// is what makes the handoff safe: the drain reader's [`PtySink::write`] (which
    /// feeds the emulator + mirrors `stdout` under the **same** lock) cannot
    /// interleave a live byte between the screen-restore escape codes; bytes the
    /// child produces *during* the redraw are read only after this unlocks and are
    /// then mirrored in order, right after the restore. No byte is lost or
    /// reordered, and full-screen programs (vim/htop) re-appear correct on the
    /// first frame instead of relying on self-redraw.
    ///
    /// The write is bounded (one screen of [`vt100::Screen::state_formatted`]
    /// escape codes, proportional to rows×cols) and synchronous — the same locking
    /// discipline `PtySink::write` already uses. `parking_lot`, never held across
    /// `.await`.
    fn attach_and_render(&self) {
        let mut out = std::io::stdout();
        self.attach_and_render_to(&mut out);
    }

    /// Inner implementation of [`attach_and_render`] with the redraw target
    /// injected, so tests can assert the under-lock ordering / non-empty redraw
    /// against a capturing sink instead of the real terminal. Production always
    /// passes `stdout`.
    ///
    /// Holds the sink lock across the `attached = true` flip AND the redraw write.
    /// `state_formatted()` is a self-contained sequence (it re-homes the cursor,
    /// repaints every cell with its attributes, and restores the cursor + input
    /// modes), so the caller need only clear the screen first.
    fn attach_and_render_to(&self, out: &mut dyn std::io::Write) {
        let mut inner = self.0.lock();
        inner.attached = true;
        // `state_formatted` rebuilds the entire current screen (cells + cursor +
        // SGR + input modes) — exactly what restores vim/htop correctly on
        // re-attach without waiting for the child to repaint.
        let redraw = inner.parser.screen().state_formatted();
        // Held-lock I/O boundary: the write/flush happen under the sink lock so the
        // redraw cannot interleave with the drain reader's live mirror (same lock).
        // This is sound because the target is the terminal only — a single bounded
        // write of one screen of escape codes; a wedged stdout would block the
        // reader, but there is no `.await` here so no async deadlock, and a "one
        // screen redraw" stall is acceptable. We `warn` on failure rather than
        // swallow it so a broken terminal is at least diagnosable.
        if let Err(e) = out.write_all(&redraw).and_then(|()| out.flush()) {
            tracing::warn!(error = %crate::agent::diagnostics::error_class(&e), "PTY re-attach redraw write to terminal failed");
        }
    }

    /// Resize the emulator's screen grid to match the PTY geometry. Called whenever
    /// the PTY is resized so the emulator stays the same size as the real terminal;
    /// otherwise the [`state_formatted`](vt100::Screen::state_formatted) redraw
    /// would be laid out for the wrong dimensions and the re-attach picture would
    /// be offset. Under the sink lock, serialized with the drain reader's feed.
    fn set_size(&self, rows: u16, cols: u16) {
        self.0.lock().parser.screen_mut().set_size(rows.max(1), cols.max(1));
    }

    /// Mark the sink detached (reader stops mirroring to `stdout`, keeps draining
    /// into the emulator + ring). Taking the lock here serializes with any in-flight
    /// reader write, so after this returns the reader will not touch `stdout` again
    /// until re-attached — the v3a "no stdout writes after the render loop resumes"
    /// invariant.
    fn detach(&self) {
        self.0.lock().attached = false;
    }

    /// Test-only: snapshot the ring contents without touching `stdout` or the
    /// `attached` flag, so unit tests can inspect what the reader buffered.
    #[cfg(test)]
    fn attach_and_snapshot_vec_for_test(&self) -> Vec<u8> {
        self.0.lock().ring.iter().copied().collect()
    }

    /// Test-only: render the emulator screen as plain text (no escape codes), so
    /// unit tests can assert what the emulator currently shows without touching
    /// `stdout`. Mirrors what a re-attach would visually restore.
    #[cfg(test)]
    fn screen_contents_for_test(&self) -> String {
        self.0.lock().parser.screen().contents()
    }

    fn last_output_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.0.lock().last_output_at
    }
}

impl std::io::Write for PtySink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Hold the lock across BOTH the routing decision and the `stdout` write so
        // a concurrent `detach()` (which flips `attached` under the same lock)
        // cannot interleave: the reader either completes this `stdout` write or
        // observes `attached == false` on its next call — never writes `stdout`
        // after detach has returned. The `stdout` write is brief (a single PTY
        // read chunk, ≤ 8 KiB) so holding the lock over it is bounded.
        let mut inner = self.0.lock();
        // Always feed the emulator so its screen grid tracks the current on-screen
        // state (used to redraw correctly on re-attach), even while detached.
        inner.parser.process(buf);
        inner.last_output_at = Some(chrono::Utc::now());
        // Always append to the ring (diagnostic / fallback), trimming the oldest
        // bytes past the cap.
        inner.ring.extend(buf.iter().copied());
        let overflow = inner.ring.len().saturating_sub(PTY_RING_CAPACITY);
        if overflow > 0 {
            inner.ring.drain(0..overflow);
        }
        if inner.attached {
            // Held-lock I/O boundary: this stdout write happens under the sink lock
            // (so a concurrent `detach()`/`attach_and_render` serializes with it).
            // The boundary is acceptable because the target is the terminal only —
            // a single PTY read chunk (≤ 8 KiB) per call; a stuck stdout would
            // block the reader/resize/detach, but there is no `.await` under the
            // lock so no async deadlock, and terminal stalls are bounded in
            // practice. `warn` on failure instead of silently dropping output so a
            // broken terminal mirror is diagnosable.
            let mut out = std::io::stdout();
            if let Err(e) = out.write_all(buf).and_then(|()| out.flush()) {
                tracing::warn!(error = %crate::agent::diagnostics::error_class(&e), "PTY live mirror write to terminal failed");
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // `stdout` is flushed inline in `write`; nothing buffered here.
        Ok(())
    }
}

/// The long-lived runtime state of a *live* PTY session (v3b).
///
/// Unlike v3a — where the reader/writer ([`PtyIo`]) were owned transiently by a
/// single passthrough and dropped on detach — v3b keeps these alive for the whole
/// session so a detached PTY can be re-attached. The persistent drain reader runs
/// from spawn until the session dies; the writer is borrowed (under a lock) by
/// whichever attach is currently active.
///
/// Held behind an `Arc` inside [`PtyShellSession`] so the cheap-clone session
/// handle in the registry carries the live runtime with it.
pub struct PtyLiveRuntime {
    /// The PTY writer (real terminal `stdin` → PTY child). Borrowed per-attach;
    /// `parking_lot::Mutex` so the attach's stdin loop and teardown serialize.
    /// Never held across `.await` (the stdin loop is synchronous).
    writer: Mutex<Option<Box<dyn std::io::Write + Send>>>,
    /// Shared sink the drain reader writes to (ring + optional `stdout` mirror).
    sink: PtySink,
    /// Stop flag for the persistent drain reader. Set only on teardown
    /// (`/kill` / chat exit / explicit reap) — NOT on detach.
    reader_stop: Arc<ReaderStop>,
    /// Set true once the drain reader observes EOF (the child genuinely exited).
    child_done: Arc<AtomicBool>,
    /// Join handle for the drain reader, taken on teardown for a bounded join.
    reader_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for PtyLiveRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyLiveRuntime")
            .field("child_done", &self.child_done.load(Ordering::Acquire))
            .field("reader_stopped", &self.reader_stop.is_stopped())
            .finish_non_exhaustive()
    }
}

/// An interactive PTY shell session: the master side of a pseudo-terminal plus
/// the spawned child's bookkeeping.
///
/// The session owns the PTY master (for resize), the child handle (for
/// `kill`/`wait`), and (v3b) a [`PtyLiveRuntime`] holding the persistent drain
/// reader, the writer, and the replay ring so a detached PTY can be re-attached.
///
/// Cloning is cheap (shared `Arc`s) so the chat registry can hold a handle while
/// an attach borrows the byte streams.
#[derive(Clone)]
pub struct PtyShellSession {
    /// Stable id (a fresh UUID, distinct from agent run / non-interactive shell
    /// ids).
    pub id: SessionId,
    /// The command line, for display in `/sessions`.
    pub command: String,
    /// When the session was spawned (for stable `/sessions` ordering/display).
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Master side of the PTY, kept for `resize`. `parking_lot::Mutex` so resize
    /// from the (sync) handoff and the session handle is race-free; never held
    /// across an `.await`.
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// The spawned child; behind a `Mutex` because `wait`/`kill` need `&mut`.
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send + Sync>>>,
    /// Process-group id (== child pid on Unix, since the PTY child is a session
    /// leader). `None` on non-Unix / if the pid was unavailable.
    pgid: Option<i32>,
    /// The long-lived drain reader + writer + replay ring (v3b).
    runtime: Arc<PtyLiveRuntime>,
}

impl PtyShellSession {
    /// Spawn an interactive command inside a fresh PTY of `size`, returning the
    /// live session handle. (v3b: unlike v3a there is no separate `PtyIo` handed
    /// out — the reader/writer are owned by the session's [`PtyLiveRuntime`], and a
    /// persistent drain reader is started here so the PTY can be detached and
    /// re-attached.)
    ///
    /// The PTY child runs directly in the workspace with the inherited parent
    /// environment and PATH.
    pub fn spawn(command: &str, workspace_dir: &Path, size: PtySize) -> Result<Self> {
        let cwd = resolve_cwd(workspace_dir);
        let id = SessionId::from_run_id(&uuid::Uuid::new_v4().to_string());

        // 2. Open the PTY pair.
        let pty = native_pty_system();
        let pair = pty
            .openpty(size)
            .map_err(|e| anyhow!("Failed to open pseudo-terminal: {e}"))?;

        // 3. Build the command: run it under a login shell so REPLs / pipelines
        //    behave, in the workspace, with the inherited env. `sh -lc <command>`
        //    mirrors the v2 background shell's `sh -c` but with a TTY attached.
        let mut builder = CommandBuilder::new("sh");
        builder.arg("-lc");
        builder.arg(command);
        builder.cwd(&cwd);

        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| anyhow!("Failed to start PTY command: {e}"))?;

        // 4. Capture pid → pgid (Unix: the PTY child is a session leader, so the
        //    pgid equals the pid; killpg of that group reaps backgrounded
        //    children too).
        let pgid = pgid_from_pid(child.process_id());

        // 5. Take the reader/writer for the passthrough; keep the master for
        //    resize. `take_writer` is valid exactly once — do it here.
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow!("Failed to clone PTY reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow!("Failed to take PTY writer: {e}"))?;

        // Capture the master's raw fd (Unix) BEFORE we move the master into the
        // session, so the passthrough's reader thread can `poll` it with a
        // timeout instead of relying on a (possibly never-arriving) EOF. The
        // cloned reader reads the same underlying fd per the `MasterPty`
        // contract, so polling this fd correctly reflects the reader's
        // readability.
        #[cfg(unix)]
        let master_fd = pair.master.as_raw_fd();

        // Drop the slave handle: the child holds its own fd, and keeping the
        // slave open here would prevent the master reader from seeing EOF when
        // the child exits.
        drop(pair.slave);

        // 6. Start the persistent drain reader (v3b). A single long-lived thread
        //    copies PTY master output into the shared sink for the WHOLE life of
        //    the session: it always fills the replay ring (so a detached PTY's
        //    output never blocks the child on a full kernel buffer) and mirrors to
        //    `stdout` only while attached. It stops only on teardown
        //    (`reader_stop`), never on detach, and never blocks unboundedly (it
        //    polls `master_fd` and re-checks the stop flag each cycle).
        let sink = PtySink::new(size);
        let reader_stop = Arc::new(ReaderStop::new());
        let child_done = Arc::new(AtomicBool::new(false));
        let reader_handle = {
            let mut reader = reader;
            let mut sink = sink.clone();
            let reader_stop = Arc::clone(&reader_stop);
            let child_done = Arc::clone(&child_done);
            // `master_fd` is a `Copy` `RawFd`; the closure captures it by copy.
            std::thread::spawn(move || {
                let outcome = read_pty_to_stdout(
                    reader.as_mut(),
                    &mut sink,
                    &reader_stop,
                    #[cfg(unix)]
                    master_fd,
                    std::time::Duration::from_millis(100),
                );
                // EOF means the child genuinely exited; a Stopped outcome is a
                // teardown signal, not a child-exit signal.
                if outcome == ReaderOutcome::Eof {
                    child_done.store(true, Ordering::Release);
                }
            })
        };

        let runtime = Arc::new(PtyLiveRuntime {
            writer: Mutex::new(Some(writer)),
            sink,
            reader_stop,
            child_done,
            reader_handle: Mutex::new(Some(reader_handle)),
        });

        Ok(Self {
            id,
            command: command.to_string(),
            started_at: chrono::Utc::now(),
            master: Arc::new(Mutex::new(pair.master)),
            child: Arc::new(Mutex::new(child)),
            pgid,
            runtime,
        })
    }

    /// Whether this session can be (re-)attached: it is still live (the drain
    /// reader has not seen EOF / the child has not exited) and still owns its
    /// writer. A dead PTY is terminal and cannot be attached.
    #[must_use]
    pub fn is_attachable(&self) -> bool {
        !self.runtime.child_done.load(Ordering::Acquire) && !self.has_exited() && self.runtime.writer.lock().is_some()
    }

    /// Whether the persistent drain reader observed the child exit (EOF). Used by
    /// the attach loop to end promptly when the child exits while attached.
    #[must_use]
    pub fn child_done_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.runtime.child_done)
    }

    #[must_use]
    pub fn last_output_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.runtime.sink.last_output_at()
    }

    /// Begin an attach: under the sink lock, mark the sink attached (the drain
    /// reader resumes mirroring PTY output to `stdout`) **and** render the
    /// emulator's current screen to `stdout` to restore on-screen context —
    /// atomically, so the redraw never interleaves with the reader's live mirror
    /// (v3b-b; replaces the v3b-a raw-ring replay).
    ///
    /// Because the redraw comes from the in-process terminal emulator
    /// ([`vt100::Screen::state_formatted`]) rather than a raw byte replay,
    /// full-screen programs (vim/htop) re-appear correct on the first frame instead
    /// of being scrambled by spliced-in historical cursor sequences.
    ///
    /// Idempotent-ish: calling while already attached just re-renders the screen.
    /// The caller is responsible for ensuring only one attach is active at a time
    /// (the chat main loop is single-threaded, so this holds).
    pub fn attach(&self) {
        self.runtime.sink.attach_and_render();
    }

    /// Mark the sink attached and return the current ring contents WITHOUT writing
    /// them to `stdout`. Test-only: lets unit tests peek the ring (and assert the
    /// drain reader filled it) without touching the real terminal. Production
    /// attach uses [`PtyShellSession::attach`], which replays under the lock.
    #[cfg(test)]
    #[must_use]
    pub fn attach_snapshot_for_test(&self) -> Vec<u8> {
        let mut inner = self.runtime.sink.0.lock();
        inner.attached = true;
        inner.ring.iter().copied().collect()
    }

    /// End an attach: stop mirroring PTY output to `stdout` (the drain reader
    /// keeps filling the ring). Serializes with the reader under the sink lock so
    /// no `stdout` write races the chat render loop's resume (v3a invariant).
    /// Does **not** kill the child — the PTY stays alive for re-attach.
    pub fn detach(&self) {
        self.runtime.sink.detach();
    }

    /// Forward a single input byte to the PTY child (real terminal `stdin` → PTY).
    /// Returns an error (never panics) if the writer has been torn down.
    pub fn write_input(&self, byte: u8) -> Result<()> {
        use std::io::Write as _;
        let mut guard = self.runtime.writer.lock();
        let writer = guard.as_mut().ok_or_else(|| anyhow!("PTY writer already closed"))?;
        writer
            .write_all(&[byte])
            .and_then(|()| writer.flush())
            .map_err(|e| anyhow!("PTY write failed: {e}"))
    }

    /// Send a redraw nudge to a full-screen child after a re-attach: toggle the
    /// PTY size (shrink one row then restore) so curses/readline programs receive
    /// `SIGWINCH` and repaint the whole screen.
    ///
    /// v3b-b: the visible re-attach picture is now restored directly from the
    /// in-process emulator ([`PtyShellSession::attach`] →
    /// [`vt100::Screen::state_formatted`]), so this nudge is no longer required for
    /// correctness — it is kept as a **secondary safeguard** so a child that tracks
    /// its own size also re-flows after the handoff (and so a re-attach matching the
    /// child's current geometry still settles cleanly). Each `resize` also keeps
    /// the emulator in sync via [`PtyShellSession::resize`]. Best-effort: a resize
    /// failure is logged, never fatal. `size` is the current host geometry to
    /// settle on.
    pub fn nudge_redraw(&self, size: PtySize) {
        // A transient smaller size, then the real size, produces two SIGWINCH
        // deliveries — the jitter most programs need to trigger a full repaint.
        let jitter = PtySize {
            rows: size.rows.saturating_sub(1).max(1),
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        if let Err(e) = self.resize(jitter) {
            tracing::debug!(error = %crate::agent::diagnostics::error_class(&e), "PTY redraw nudge (jitter resize) failed");
        }
        if let Err(e) = self.resize(size) {
            tracing::debug!(error = %crate::agent::diagnostics::error_class(&e), "PTY redraw nudge (restore resize) failed");
        }
    }

    /// Maximum time [`reap_reader`] will wait for the drain-reader thread to finish
    /// before giving up and detaching it (v3b-a blocker-2: a truly bounded join).
    const REAP_JOIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(1);

    /// Tear down the persistent drain reader: request it to stop, drop the writer
    /// (so the slave observes EOF), and **bounded**-join the reader thread.
    /// Idempotent and never panics. Called from the kill paths (`/kill`, chat exit)
    /// and when reaping a dead PTY, so no drain thread outlives the session.
    ///
    /// v3b-a blocker-2: the previous `handle.join()` was unbounded despite the
    /// "bounded" claim — if the reader were stuck (a backgrounded grandchild
    /// reparented to init holds the slave open so no EOF arrives, or the non-Unix
    /// blocking-read fallback is parked on an idle orphan), `join()` could hang the
    /// whole chat. We now poll [`std::thread::JoinHandle::is_finished`] up to
    /// [`Self::REAP_JOIN_DEADLINE`]; past the deadline we detach the (already
    /// stopped) thread and `warn`, so the chat main path never blocks. The reader
    /// is flagged to stop and will exit at its next poll cycle (≤ its poll
    /// timeout), so the detached thread is a parked, short-lived leak at worst.
    pub fn reap_reader(&self) {
        // 1. Ask the reader to stop at its next poll cycle.
        self.runtime.reader_stop.stop();
        // 2. Drop the writer so the slave end can observe EOF.
        let _ = self.runtime.writer.lock().take();
        // 3. Detach the sink defensively so nothing mirrors to stdout during
        //    teardown.
        self.runtime.sink.detach();
        // 4. Bounded join: never block the chat path on a stuck reader.
        let handle = self.runtime.reader_handle.lock().take();
        if let Some(handle) = handle {
            bounded_join(handle, Self::REAP_JOIN_DEADLINE);
        }
    }

    /// Resize the PTY to match the host terminal, keeping the in-process emulator
    /// the same size (v3b-b). The emulator must track the PTY geometry or its
    /// re-attach redraw ([`vt100::Screen::state_formatted`]) would be laid out for
    /// the wrong dimensions and the restored picture would be offset / wrapped
    /// wrong. We resize the emulator first (infallible) then the PTY master; both
    /// are cheap, synchronous, and never panic (master errors are returned). Safe
    /// to call while attached.
    pub fn resize(&self, size: PtySize) -> Result<()> {
        // Keep the emulator grid in step with the PTY winsize. Done before the
        // master resize so even if the master resize errors the emulator already
        // reflects the host geometry we are settling on.
        self.runtime.sink.set_size(size.rows, size.cols);
        self.master
            .lock()
            .resize(size)
            .map_err(|e| anyhow!("PTY resize failed: {e}"))
    }

    /// Whether the child has exited (non-blocking poll). Never panics.
    #[must_use]
    pub fn has_exited(&self) -> bool {
        matches!(self.child.lock().try_wait(), Ok(Some(_)))
    }

    /// Synchronously terminate the session's process group **without** the async
    /// SIGTERM grace window — used on the detach path inside the blocking
    /// passthrough thread, where we must close the PTY master promptly so the
    /// output-reader thread sees EOF and stops writing to `stdout` *before* the
    /// chat render loop resumes (otherwise lingering PTY bytes would corrupt the
    /// restored TUI).
    ///
    /// Sends `SIGHUP` then `SIGKILL` to the whole group (Unix), or kills the
    /// direct child (non-Unix). Idempotent and never panics: a group that is
    /// already gone (`ESRCH`) is treated as success.
    #[allow(unsafe_code)]
    pub fn kill_now(&self) {
        #[cfg(unix)]
        {
            if let Some(pgid) = self.pgid {
                // Invariant upheld by `pgid_from_pid`: `pgid > 0`, so `killpg`
                // can never signal our own group (0) or the whole system (-1).
                debug_assert!(pgid > 0, "kill_now: pgid must be strictly positive");
                // SAFETY: `killpg` is an async-signal-safe libc call that only
                // signals the process group `pgid`; it dereferences no pointers
                // and has no memory-safety preconditions. `pgid` is the session
                // leader pid of our own PTY child (a descendant group), so
                // signalling it is sound.
                let (hup, kill) = unsafe {
                    // SIGHUP first (the natural "terminal closed" signal for an
                    // interactive shell), then SIGKILL to guarantee teardown.
                    let hup: i32 = libc::killpg(pgid, libc::SIGHUP);
                    let kill: i32 = libc::killpg(pgid, libc::SIGKILL);
                    (hup, kill)
                };
                // Don't silently ignore the result: a non-`ESRCH` failure means
                // the group may still be alive. We do NOT rely on the kill to
                // unblock anything (the reader thread is stopped via its own
                // stop flag with a bounded `poll`, never an unbounded EOF wait),
                // but a surprising errno is worth a log line for diagnosis.
                if kill != 0 {
                    let errno = std::io::Error::last_os_error();
                    // `ESRCH` (no such process group) is the expected, benign
                    // outcome when the child already exited — not worth warning.
                    if errno.raw_os_error() != Some(libc::ESRCH) && !super::shell::process_group_leader_is_gone(pgid) {
                        tracing::warn!(
                            pgid,
                            hup,
                            kill,
                            error = %crate::agent::diagnostics::error_class(&errno),
                            "kill_now: killpg(SIGKILL) did not succeed"
                        );
                    }
                }
                return;
            }
        }
        // No pgid: best-effort kill of the direct child. Bind the result first so
        // the `MutexGuard` temporary does not live across the `if let` body
        // (clippy::significant_drop_in_scrutinee).
        let kill_result = self.child.lock().kill();
        if let Err(e) = kill_result {
            tracing::warn!(error = %crate::agent::diagnostics::error_class(&e), "kill_now: direct child kill failed");
        }
    }

    /// Terminate the session's whole process group (Unix) or the direct child
    /// (non-Unix), so `/kill` and chat exit leave no orphans.
    ///
    /// Idempotent and never panics: if the child has already exited the kill is
    /// a no-op (`killpg` returns `ESRCH`, mapped to `Ok` by
    /// [`super::shell::kill_process_group`]).
    pub async fn kill(&self) -> Result<()> {
        // v3b-a blocker-2: SIGNAL THE CHILD FIRST, then reap the reader.
        //
        // The old order (reap_reader → kill_process_group) could hang: if the
        // reader were stuck waiting on a slave fd held open by a reparented
        // grandchild, the (then-unbounded) join blocked *before* we ever killed the
        // group. Killing the group first makes the master hit EOF promptly, so the
        // reader's poll returns and its (now bounded) join completes fast; even in
        // the pathological case the bounded join in `reap_reader` guarantees we
        // never block the chat path.
        let kill_result: Result<()> = {
            #[cfg(unix)]
            {
                if let Some(pgid) = self.pgid {
                    super::shell::kill_process_group(pgid).await
                } else {
                    // No pgid: fall back to the direct child killer. Bind the
                    // result so the `MutexGuard` is dropped before the `?`/return.
                    let r = self.child.lock().kill();
                    r.map_err(|e| anyhow!("PTY child kill failed: {e}"))
                }
            }
            #[cfg(not(unix))]
            {
                let r = self.child.lock().kill();
                r.map_err(|e| anyhow!("PTY child kill failed: {e}"))
            }
        };

        // Now stop the persistent drain reader and bounded-join it, dropping the
        // writer fd. The child is already signalled, so the master is at (or
        // racing toward) EOF and the reader stops promptly. Reaping is best-effort
        // teardown: do it even if the kill signal errored so no thread/fd leaks,
        // then surface the kill error.
        self.reap_reader();
        kill_result
    }
}

/// Resolve the workspace directory to a canonical cwd, falling back to the
/// original on a canonicalize error (mirrors the v2 background shell).
fn resolve_cwd(workspace_dir: &Path) -> PathBuf {
    workspace_dir
        .canonicalize()
        .unwrap_or_else(|_| workspace_dir.to_path_buf())
}

/// Derive the process-group id to signal from a spawned PTY child's pid.
///
/// On Unix `portable-pty` makes the child a session leader (`setsid`), so its
/// pgid equals its pid; signalling that group with `killpg` reaps backgrounded
/// descendants too. On non-Unix there is no portable pgid, so this returns
/// `None` and [`PtyShellSession::kill`] falls back to the child killer.
#[cfg(unix)]
fn pgid_from_pid(pid: Option<u32>) -> Option<i32> {
    // pid fits in i32 on every supported Unix; this is the conventional pid_t
    // representation used by killpg. Reject non-positive values defensively: a
    // pgid of `0` means "the caller's own process group" and a negative value is
    // never a valid pid — either would turn a later `killpg` into a catastrophe
    // (signalling *our own* group / the whole system). Only a strictly positive
    // pgid is ever signalled.
    pid.and_then(|pid| i32::try_from(pid).ok()).filter(|&pgid| pgid > 0)
}

#[cfg(not(unix))]
fn pgid_from_pid(_pid: Option<u32>) -> Option<i32> {
    None
}

/// Classify a single input byte during an attached PTY passthrough.
///
/// Pure function (no I/O), trivially unit-testable: it encodes the v3 detach
/// contract — `Ctrl-]` detaches; `Ctrl-C`/`Ctrl-D` and everything else are
/// forwarded to the PTY child verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputByte {
    /// `Ctrl-]` — leave the passthrough and restore the chat TUI.
    Detach,
    /// Forward this byte to the PTY child unchanged (includes Ctrl-C / Ctrl-D).
    Forward,
}

/// Classify an input byte per the detach contract (see [`InputByte`]).
#[must_use]
pub const fn classify_input_byte(byte: u8) -> InputByte {
    if byte == DETACH_BYTE {
        InputByte::Detach
    } else {
        InputByte::Forward
    }
}

/// A cancellation flag for the PTY passthrough's output-reader thread.
///
/// The reader thread copies PTY output → `stdout`. On detach we MUST be able to
/// stop and `join` it *without waiting for EOF*: a backgrounded grandchild that
/// is reparented to init (so `killpg` can't reach it) can keep the slave PTY fd
/// open forever, and a reader blocked in `read()` would then never return —
/// freezing the whole chat. To stay interruptible the reader polls the master fd
/// with a short timeout and checks [`ReaderStop::is_stopped`] every cycle,
/// exiting promptly when asked even if no EOF ever arrives.
#[derive(Debug, Default)]
pub struct ReaderStop(AtomicBool);

impl ReaderStop {
    /// A fresh, not-yet-stopped flag.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// Request the reader thread to stop at its next poll cycle.
    pub fn stop(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether a stop has been requested.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Outcome of the interruptible reader loop, so the caller (and tests) can tell
/// *why* it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderOutcome {
    /// The PTY master reached EOF (child exited / master closed).
    Eof,
    /// A stop was requested via [`ReaderStop::stop`] (e.g. detach).
    Stopped,
    /// `stdout` could not be written (terminal gone); nothing more to do.
    StdoutGone,
}

/// Join a thread within a bounded deadline, returning `true` if it finished (and
/// was joined) and `false` if the deadline expired (the handle is dropped, which
/// detaches the thread).
///
/// v3b-a blocker-2: the drain-reader teardown must never block the chat main path
/// indefinitely. The reader is always *flagged* to stop before this is called, so
/// it normally finishes near-instantly; this guard exists purely so a
/// pathologically stuck reader (e.g. a reparented grandchild holding the slave PTY
/// open on a platform without the poll-interruptible path) cannot freeze chat —
/// we give up and let the OS reap the parked thread when the process exits.
///
/// Pure (no chat state) and never panics, so it is unit-testable on its own.
fn bounded_join(handle: std::thread::JoinHandle<()>, deadline: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    while !handle.is_finished() {
        if start.elapsed() >= deadline {
            tracing::warn!(
                deadline_ms = deadline.as_millis(),
                "PTY drain reader did not stop within the deadline; detaching thread to avoid blocking chat (it is flagged to stop and will exit shortly)"
            );
            // Drop `handle` without joining: detaches the thread.
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // Finished within the deadline: join is now non-blocking.
    let _ = handle.join();
    true
}

/// Copy PTY output → `out` until EOF, a stop request, or a `stdout` write
/// failure, **never blocking unboundedly**.
///
/// On Unix it `poll`s `master_fd` with a `poll_timeout` so every cycle re-checks
/// `stop`; this is what makes detach safe even when an orphaned grandchild holds
/// the slave open (no EOF will ever come). `reader` must read the same
/// underlying fd as `master_fd` (it does: the cloned reader and the master share
/// the kernel pty master per the `portable-pty` contract).
///
/// On non-Unix (no `master_fd`) it falls back to a plain blocking `read` loop;
/// the stop flag is still honoured between reads but a fully-idle orphan can
/// delay the final `join` until the next byte/EOF (documented platform
/// limitation).
///
/// Pure of chat state and never panics: returns a [`ReaderOutcome`] for the
/// caller to log/branch on.
#[allow(unsafe_code)]
pub fn read_pty_to_stdout(
    reader: &mut dyn std::io::Read,
    out: &mut dyn std::io::Write,
    stop: &ReaderStop,
    #[cfg(unix)] master_fd: Option<std::os::fd::RawFd>,
    poll_timeout: std::time::Duration,
) -> ReaderOutcome {
    let mut buf = [0u8; 8192];
    // Clamp the poll timeout to a sane i32 millisecond count for `libc::poll`.
    // Only the unix `poll` path consumes it; on other targets the blocking read
    // provides its own backpressure, so the value is unused there.
    #[cfg(unix)]
    let timeout_ms = i32::try_from(poll_timeout.as_millis()).unwrap_or(100).max(1);
    #[cfg(not(unix))]
    let _ = poll_timeout;
    loop {
        if stop.is_stopped() {
            return ReaderOutcome::Stopped;
        }

        // On Unix, wait (bounded) for the master to be readable so each cycle
        // re-checks the stop flag without blocking on a read that may never
        // return (orphaned grandchild holding the slave open).
        #[cfg(unix)]
        if let Some(fd) = master_fd {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `poll` reads/writes exactly the one `pollfd` we pass
            // (`nfds = 1`); `&raw mut pfd` points to a live local valid for the
            // duration of the call. `poll` dereferences nothing else and has no
            // memory-safety preconditions. `timeout_ms >= 1` bounds the wait so
            // the surrounding loop re-checks `stop` promptly.
            let rc = unsafe { libc::poll(&raw mut pfd, 1, timeout_ms) };
            if rc <= 0 {
                // Timeout (0) or EINTR (<0): loop to re-check `stop`.
                continue;
            }
            if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 && pfd.revents & libc::POLLIN == 0 {
                // Hung up with no pending data → master closed / child gone.
                return ReaderOutcome::Eof;
            }
            // Readable: fall through to the read below.
        }

        match reader.read(&mut buf) {
            Ok(0) => return ReaderOutcome::Eof, // EOF: child exited / master closed
            Ok(n) => {
                let chunk = buf.get(..n).unwrap_or(&buf);
                if out.write_all(chunk).is_err() || out.flush().is_err() {
                    return ReaderOutcome::StdoutGone;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // On a non-blocking-ish wakeup with nothing to read, keep looping so
            // the stop flag is re-checked rather than treating it as a hard EOF.
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return ReaderOutcome::Eof,
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests;
