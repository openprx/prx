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
use crate::security::{SecurityPolicy, SideEffectGate};
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
    /// time (e.g. it is wedged inside a slow `insert_before`, or the TUI was
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
    pub fn acquire(control: Arc<HandoffControl>, redraw_nudge: Option<Box<dyn Fn() + Send>>) -> Option<Self> {
        // Block (bounded) until the render loop confirms it has parked.
        if control.pause_and_wait(PAUSE_ACK_TIMEOUT) {
            return Some(Self { control, redraw_nudge });
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
        // own control sequences to `stdout`) concurrently with the
        // `enable_raw_mode` / `EnableBracketedPaste` below, or step into
        // crossterm `poll`/`read` before raw mode is re-asserted. So:
        //
        //   1. restore terminal modes  (render loop STILL paused)
        //   2. arm force_redraw
        //   3. unpause                  (render loop may now draw)
        //   4. nudge
        //
        // After step 3 we write NO further stdout control sequences.

        // 1. Defensively re-assert the chat terminal modes the PTY child may
        //    have clobbered, while the render loop is still parked so we are the
        //    sole writer of these escape/kernel operations. Best-effort — we are
        //    on the restoration path and have no caller to surface errors to.
        let _ = crossterm::terminal::enable_raw_mode();
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
/// - **always** appends bytes to a bounded ring buffer (so detached PTYs keep
///   draining — a full kernel pty buffer would otherwise block the child — and
///   so a re-attach can replay recent context); and
/// - **when attached**, also writes the bytes straight to the real terminal
///   `stdout` (so the attached user sees live output).
///
/// Routing (the attached check + ring append + optional `stdout` write) happens
/// under a single `parking_lot::Mutex`, and detach flips `attached` under that
/// **same** lock. This closes the v3a invariant precisely: once detach has set
/// `attached = false`, the reader's *next* write observes it; a write already
/// in-flight completes its `stdout` write before detach can flip the flag (they
/// serialize on the mutex). So the reader is guaranteed to have stopped writing
/// `stdout` by the time detach returns — before the `PtyHandoffGuard` resumes the
/// chat render loop. Synchronous `parking_lot` only; never held across `.await`.
#[derive(Debug)]
struct SinkInner {
    /// Whether the session is currently attached (reader should mirror to
    /// `stdout`). Guarded by the same lock as the ring so flip + write serialize.
    attached: bool,
    /// Bounded raw-byte scrollback ring. `VecDeque` for O(1) push-back /
    /// pop-front; capped at [`PTY_RING_CAPACITY`].
    ring: std::collections::VecDeque<u8>,
}

/// Handle to a PTY's drain sink, shared between the drain reader and the session.
///
/// Cloning shares the same `Arc<Mutex<SinkInner>>`.
#[derive(Clone, Debug)]
struct PtySink(Arc<Mutex<SinkInner>>);

impl PtySink {
    /// A fresh, detached sink with an empty ring.
    fn new() -> Self {
        Self(Arc::new(Mutex::new(SinkInner {
            attached: false,
            ring: std::collections::VecDeque::with_capacity(8192),
        })))
    }

    /// Mark the sink attached (reader will mirror to `stdout`). Returns the
    /// current ring contents so the caller can replay them to restore context.
    fn attach_and_snapshot(&self) -> Vec<u8> {
        let mut inner = self.0.lock();
        inner.attached = true;
        inner.ring.iter().copied().collect()
    }

    /// Mark the sink detached (reader stops mirroring to `stdout`, keeps draining
    /// into the ring). Taking the lock here serializes with any in-flight reader
    /// write, so after this returns the reader will not touch `stdout` again until
    /// re-attached — the v3a "no stdout writes after the render loop resumes"
    /// invariant.
    fn detach(&self) {
        self.0.lock().attached = false;
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
        // Always append to the ring, trimming the oldest bytes past the cap.
        inner.ring.extend(buf.iter().copied());
        let overflow = inner.ring.len().saturating_sub(PTY_RING_CAPACITY);
        if overflow > 0 {
            inner.ring.drain(0..overflow);
        }
        if inner.attached {
            let mut out = std::io::stdout();
            let _ = out.write_all(buf);
            let _ = out.flush();
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
    /// Security: the command is authorized through the **same**
    /// [`SideEffectGate`] the interactive shell tool uses, so high-risk commands
    /// (`rm -rf /`, `mkfs`, …) are still blocked / require a grant even though
    /// the operator typed `/pty`. The child runs in the workspace directory with
    /// a hardened `PATH` + the same safe-env allow-list as the v2 background
    /// shell (no secrets leak into the PTY).
    pub fn spawn(command: &str, security: &Arc<SecurityPolicy>, size: PtySize) -> Result<Self> {
        // 1. Security gate — identical policy to the shell tool. We never bypass
        //    the gate just because `/pty` was typed interactively.
        if security.is_rate_limited() {
            return Err(anyhow!("Rate limit exceeded: too many actions in the last hour"));
        }
        SideEffectGate::new(security.as_ref())
            .authorize_command_execution("shell", command, None)
            .map_err(|reason| anyhow!("{reason}"))?;
        if !security.record_action() {
            return Err(anyhow!("Rate limit exceeded: action budget exhausted"));
        }

        let cwd = resolve_cwd(&security.workspace_dir);
        let id = SessionId::from_run_id(&uuid::Uuid::new_v4().to_string());

        // 2. Open the PTY pair.
        let pty = native_pty_system();
        let pair = pty
            .openpty(size)
            .map_err(|e| anyhow!("Failed to open pseudo-terminal: {e}"))?;

        // 3. Build the command: run it under a login shell so REPLs / pipelines
        //    behave, in the workspace, with a hardened env. `sh -lc <command>`
        //    mirrors the v2 background shell's `sh -c` but with a TTY attached.
        let mut builder = CommandBuilder::new("sh");
        builder.arg("-lc");
        builder.arg(command);
        builder.cwd(&cwd);
        apply_safe_env(&mut builder);

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
        let sink = PtySink::new();
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

    /// Begin an attach: mark the sink attached (the drain reader resumes
    /// mirroring PTY output to `stdout`) and return the current ring contents so
    /// the caller can replay them to restore on-screen context.
    ///
    /// Idempotent-ish: calling while already attached just re-snapshots. The
    /// caller is responsible for ensuring only one attach is active at a time
    /// (the chat main loop is single-threaded, so this holds).
    #[must_use]
    pub fn attach(&self) -> Vec<u8> {
        self.runtime.sink.attach_and_snapshot()
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
    /// `SIGWINCH` and repaint the whole screen, covering any artefacts left by the
    /// raw-byte ring replay (v3b-a画面恢复 scheme (a)+(c)). Best-effort: a resize
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
            tracing::debug!(error = %e, "PTY redraw nudge (jitter resize) failed");
        }
        if let Err(e) = self.resize(size) {
            tracing::debug!(error = %e, "PTY redraw nudge (restore resize) failed");
        }
    }

    /// Tear down the persistent drain reader: request it to stop, drop the writer
    /// (so the slave observes EOF), and bounded-join the reader thread. Idempotent
    /// and never panics. Called from the kill paths (`/kill`, chat exit) and when
    /// reaping a dead PTY, so no drain thread outlives the session.
    pub fn reap_reader(&self) {
        self.runtime.reader_stop.stop();
        // Drop the writer so the slave end can observe EOF.
        let _ = self.runtime.writer.lock().take();
        // Detach the sink defensively so nothing mirrors to stdout during teardown.
        self.runtime.sink.detach();
        let handle = self.runtime.reader_handle.lock().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    /// Resize the PTY to match the host terminal. Cheap, synchronous, never
    /// panics (errors are returned). Safe to call while attached.
    pub fn resize(&self, size: PtySize) -> Result<()> {
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
                    let hup = libc::killpg(pgid, libc::SIGHUP);
                    let kill = libc::killpg(pgid, libc::SIGKILL);
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
                    if errno.raw_os_error() != Some(libc::ESRCH) {
                        tracing::warn!(
                            pgid,
                            hup_rc = hup,
                            kill_rc = kill,
                            error = %errno,
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
            tracing::warn!(error = %e, "kill_now: direct child kill failed");
        }
    }

    /// Terminate the session's whole process group (Unix) or the direct child
    /// (non-Unix), so `/kill` and chat exit leave no orphans.
    ///
    /// Idempotent and never panics: if the child has already exited the kill is
    /// a no-op (`killpg` returns `ESRCH`, mapped to `Ok` by
    /// [`super::shell::kill_process_group`]).
    pub async fn kill(&self) -> Result<()> {
        // v3b: stop the persistent drain reader and join it (bounded) before/while
        // we kill the group, so no drain thread or writer fd outlives the session.
        self.reap_reader();
        #[cfg(unix)]
        {
            if let Some(pgid) = self.pgid {
                return super::shell::kill_process_group(pgid).await;
            }
        }
        // No pgid (non-Unix, or pid unavailable): fall back to the child killer.
        let mut child = self.child.lock();
        child.kill().map_err(|e| anyhow!("PTY child kill failed: {e}"))
    }
}

/// Resolve the workspace directory to a canonical cwd, falling back to the
/// original on a canonicalize error (mirrors the v2 background shell).
fn resolve_cwd(workspace_dir: &Path) -> PathBuf {
    workspace_dir
        .canonicalize()
        .unwrap_or_else(|_| workspace_dir.to_path_buf())
}

/// Apply the hardened-PATH + safe-env baseline to a [`CommandBuilder`], mirroring
/// the v2 background shell's allow-list so no API keys / secrets leak into the
/// interactive PTY (CWE-200).
fn apply_safe_env(builder: &mut CommandBuilder) {
    builder.env_clear();
    for var in SAFE_ENV_VARS {
        if let Ok(val) = std::env::var(var) {
            builder.env(var, val);
        }
    }
    builder.env("PATH", HARDENED_PATH);
    // A sane terminal type so curses programs (vim/top) render; the host's TERM
    // is preferred when present (set above via the allow-list) but default to a
    // widely-supported value otherwise.
    if std::env::var("TERM").is_err() {
        builder.env("TERM", "xterm-256color");
    }
}

/// Environment variables safe to pass to an interactive PTY shell. Mirrors the
/// v2 background shell allow-list: only functional variables, never secrets.
const SAFE_ENV_VARS: &[&str] = &[
    "PATH", "HOME", "TERM", "LANG", "LC_ALL", "LC_CTYPE", "USER", "SHELL", "TMPDIR",
];

/// Hardened PATH for interactive PTY commands (no user-writable directories),
/// matching the v2 background shell secure default.
#[cfg(not(target_os = "windows"))]
const HARDENED_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
#[cfg(target_os = "windows")]
const HARDENED_PATH: &str = r"C:\Windows\System32;C:\Windows;C:\Windows\System32\Wbem";

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
mod tests {
    use super::*;
    use crate::security::AutonomyLevel;

    fn auto_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            allowed_commands: vec!["*".into()],
            ..SecurityPolicy::default()
        })
    }

    // ── Byte classification (detach contract) ────────────────────────────────

    #[test]
    fn ctrl_rbracket_detaches() {
        assert_eq!(classify_input_byte(DETACH_BYTE), InputByte::Detach);
        assert_eq!(classify_input_byte(0x1d), InputByte::Detach);
    }

    #[test]
    fn ctrl_c_and_d_pass_through() {
        // Ctrl-C / Ctrl-D must reach the PTY child, never detach.
        assert_eq!(classify_input_byte(CTRL_C), InputByte::Forward);
        assert_eq!(classify_input_byte(CTRL_D), InputByte::Forward);
    }

    #[test]
    fn ordinary_bytes_forward() {
        for b in [b'a', b'Z', b'0', b'\n', b'\r', 0x1b /* Esc */, b' '] {
            assert_eq!(classify_input_byte(b), InputByte::Forward, "byte {b:#x}");
        }
    }

    // ── HandoffControl pause / ack / resume ──────────────────────────────────

    #[test]
    fn control_starts_resumed() {
        let c = HandoffControl::new();
        assert!(!c.is_paused());
        assert!(!c.take_force_redraw());
    }

    #[test]
    fn pause_and_wait_returns_when_render_loop_acks() {
        let c = Arc::new(HandoffControl::new());
        let c2 = Arc::clone(&c);
        // Simulate a render loop that, once it sees the pause, acks.
        let handle = std::thread::spawn(move || {
            for _ in 0..1000 {
                if c2.is_paused() {
                    c2.ack_paused();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        assert!(
            c.pause_and_wait(std::time::Duration::from_secs(2)),
            "ack should arrive before timeout"
        );
        assert!(c.is_paused());
        handle.join().expect("test: render-loop sim joins");
    }

    #[test]
    fn pause_and_wait_times_out_without_ack() {
        let c = HandoffControl::new();
        // No one acks → proceeds on timeout, returns false, but still paused.
        assert!(!c.pause_and_wait(std::time::Duration::from_millis(30)));
        assert!(c.is_paused());
    }

    #[test]
    fn resume_with_redraw_clears_pause_and_sets_force_redraw() {
        let c = HandoffControl::new();
        assert!(!c.pause_and_wait(std::time::Duration::from_millis(10)));
        assert!(c.is_paused());
        c.resume_with_redraw();
        assert!(!c.is_paused());
        // The render loop takes the force-redraw flag exactly once.
        assert!(c.take_force_redraw());
        assert!(!c.take_force_redraw());
    }

    // ── PtyHandoffGuard RAII restoration ─────────────────────────────────────

    /// Spawn a short-lived "render loop" that acks the pause so
    /// [`PtyHandoffGuard::acquire`] succeeds (it now refuses to attach if the
    /// pause is never acknowledged).
    fn spawn_acking_render_loop(control: &Arc<HandoffControl>) -> std::thread::JoinHandle<()> {
        let c = Arc::clone(control);
        std::thread::spawn(move || {
            for _ in 0..1000 {
                if c.is_paused() {
                    c.ack_paused();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })
    }

    #[test]
    fn guard_drop_resumes_and_forces_redraw() {
        let control = Arc::new(HandoffControl::new());
        let nudged = Arc::new(AtomicBool::new(false));
        let nudged2 = Arc::clone(&nudged);
        let acker = spawn_acking_render_loop(&control);
        {
            let guard = PtyHandoffGuard::acquire(
                Arc::clone(&control),
                Some(Box::new(move || nudged2.store(true, Ordering::Release))),
            )
            .expect("test: ack arrives so acquire succeeds");
            // During the handoff the render loop is paused.
            assert!(control.is_paused());
            drop(guard);
        }
        acker.join().expect("test: acker joins");
        // Drop restored everything: resumed, force-redraw set, nudge fired.
        assert!(!control.is_paused(), "guard drop must resume the render loop");
        assert!(control.take_force_redraw(), "guard drop must force a redraw");
        assert!(nudged.load(Ordering::Acquire), "guard drop must nudge the renderer");
    }

    #[test]
    fn guard_restores_even_on_panic_unwind() {
        // The RAII contract: a panic unwinding through the handoff still runs
        // Drop and restores the render loop. We catch the unwind so the test
        // process survives and can assert the post-conditions.
        let control = Arc::new(HandoffControl::new());
        let acker = spawn_acking_render_loop(&control);
        let control2 = Arc::clone(&control);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard =
                PtyHandoffGuard::acquire(Arc::clone(&control2), None).expect("test: ack arrives so acquire succeeds");
            assert!(control2.is_paused());
            panic!("test: simulate a fault during PTY handoff");
        }));
        acker.join().expect("test: acker joins");
        assert!(result.is_err(), "the closure panicked");
        assert!(
            !control.is_paused(),
            "terminal must be restored (render loop resumed) even after a panic"
        );
        assert!(control.take_force_redraw());
    }

    #[test]
    fn acquire_aborts_when_render_loop_never_parks() {
        // P0-D: if the render loop never acks the pause, `acquire` must return
        // `None` and leave the control un-paused (handoff refused, terminal
        // untouched), never proceed with a half-done handoff.
        let control = Arc::new(HandoffControl::new());
        // No acker thread → the ack will time out.
        let guard = PtyHandoffGuard::acquire(Arc::clone(&control), None);
        assert!(guard.is_none(), "acquire must refuse when the render loop never parks");
        assert!(
            !control.is_paused(),
            "a refused attach must leave the render loop resumed, not stuck paused"
        );
    }

    #[test]
    fn guard_drop_restores_terminal_mode_before_unpausing() {
        // P0-C ordering: the guard must restore terminal modes WHILE the render
        // loop is still paused, then unpause. We cannot observe the crossterm
        // calls directly here, but we CAN assert the externally-visible ordering
        // contract `resume_with_redraw` guarantees: force_redraw is armed before
        // (or atomically with) `paused` being cleared, so a render loop observing
        // `!is_paused()` always also sees `force_redraw == true`.
        let control = Arc::new(HandoffControl::new());
        let acker = spawn_acking_render_loop(&control);
        let guard =
            PtyHandoffGuard::acquire(Arc::clone(&control), None).expect("test: ack arrives so acquire succeeds");
        assert!(control.is_paused());
        drop(guard);
        acker.join().expect("test: acker joins");
        // After drop: unpaused AND force_redraw armed (so the render loop repaints
        // the chrome rather than leaving PTY residue / a blank viewport).
        assert!(!control.is_paused(), "guard drop must unpause");
        assert!(control.take_force_redraw(), "force_redraw must be set on resume");
    }

    // ── Interruptible reader (P0-A: bounded stop, no EOF dependency) ──────────

    #[test]
    fn reader_stop_flag_round_trips() {
        let s = ReaderStop::new();
        assert!(!s.is_stopped());
        s.stop();
        assert!(s.is_stopped());
    }

    #[test]
    fn reader_returns_eof_on_closed_pipe() {
        // A reader whose source is already at EOF must return `Eof` promptly
        // without needing the stop flag. (No fd → blocking-read fallback path,
        // exercised on every platform.)
        use std::io::Cursor;
        let mut reader = Cursor::new(Vec::<u8>::new()); // empty → immediate EOF
        let mut out: Vec<u8> = Vec::new();
        let stop = ReaderStop::new();
        let outcome = read_pty_to_stdout(
            &mut reader,
            &mut out,
            &stop,
            #[cfg(unix)]
            None,
            std::time::Duration::from_millis(10),
        );
        assert_eq!(outcome, ReaderOutcome::Eof);
        assert!(out.is_empty());
    }

    #[test]
    fn reader_copies_then_eof() {
        use std::io::Cursor;
        let mut reader = Cursor::new(b"hello".to_vec());
        let mut out: Vec<u8> = Vec::new();
        let stop = ReaderStop::new();
        let outcome = read_pty_to_stdout(
            &mut reader,
            &mut out,
            &stop,
            #[cfg(unix)]
            None,
            std::time::Duration::from_millis(10),
        );
        assert_eq!(outcome, ReaderOutcome::Eof);
        assert_eq!(out, b"hello");
    }

    /// A `Read` that NEVER returns data or EOF — it parks each `read` until told
    /// to stop, modelling an orphaned grandchild that holds the slave PTY open so
    /// the master never sees EOF. The interruptible reader must still exit
    /// (bounded) when the stop flag is set, instead of blocking forever.
    struct NeverEofReader {
        released: Arc<AtomicBool>,
    }
    impl std::io::Read for NeverEofReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            // Block until released, then report a spurious WouldBlock so the
            // reader loops back and observes the stop flag (mirrors the unix
            // poll-timeout path which never even calls read while idle).
            while !self.released.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        }
    }

    #[test]
    fn reader_stops_without_eof_when_flag_set() {
        // P0-A core invariant: the reader exits on the stop flag WITHOUT EOF, so
        // a detach can always `join` it in bounded time even with an orphan
        // holding the slave open. We run the reader on a thread and assert it
        // joins quickly after we stop it.
        let released = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(ReaderStop::new());
        let stop_thread = Arc::clone(&stop);
        let mut reader = NeverEofReader {
            released: Arc::clone(&released),
        };
        let handle = std::thread::spawn(move || {
            let mut out: Vec<u8> = Vec::new();
            read_pty_to_stdout(
                &mut reader,
                &mut out,
                &stop_thread,
                // `None` fd → blocking-read fallback, so the stop flag (checked
                // between reads) is the ONLY exit. This is the worst case and
                // proves the flag alone bounds the loop.
                #[cfg(unix)]
                None,
                std::time::Duration::from_millis(20),
            )
        });
        // Let the reader park in `read` a moment, then request stop + release the
        // blocked read so it loops once and sees the flag.
        std::thread::sleep(std::time::Duration::from_millis(30));
        stop.stop();
        released.store(true, Ordering::Release);
        // Join must complete well within a generous bound (no infinite block).
        let start = std::time::Instant::now();
        let outcome = loop {
            if handle.is_finished() {
                break handle.join().expect("test: reader thread joins");
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "reader did not stop within bound — would freeze chat on detach"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(outcome, ReaderOutcome::Stopped);
    }

    // ── PTY spawn + interaction (Unix; needs a real /dev/ptmx) ───────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn high_risk_command_is_rejected() {
        let sec = auto_security();
        let err = PtyShellSession::spawn("rm -rf /", &sec, PtySize::default())
            .err()
            .expect("test: high-risk denied before any PTY is opened");
        assert!(!err.to_string().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pty_echoes_input_and_exits_cleanly() {
        // v3b: the persistent drain reader fills the ring; write via the session
        // and observe the echo via an attach snapshot of the ring.
        let sec = auto_security();
        let session = PtyShellSession::spawn("cat", &sec, PtySize::default()).expect("test: spawn cat in a PTY");

        // Write a line; `cat` echoes it back through the PTY → the drain reader
        // appends it to the ring.
        for &b in b"hello-pty\n" {
            session.write_input(b).expect("test: write to PTY");
        }

        // Poll the ring (via attach snapshot) until the echo appears.
        let mut saw = false;
        for _ in 0..50 {
            let ring = session.attach();
            if String::from_utf8_lossy(&ring).contains("hello-pty") {
                saw = true;
                break;
            }
            session.detach();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(saw, "PTY echoed the written line back into the ring");

        // Terminating the group exits `cat` cleanly; kill is idempotent and reaps
        // the drain reader.
        session.kill().await.expect("test: kill PTY group");
        for _ in 0..50 {
            if session.has_exited() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(session.has_exited(), "PTY child terminated after kill");
        session.kill().await.expect("test: idempotent kill");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pty_resize_is_accepted() {
        let sec = auto_security();
        let session =
            PtyShellSession::spawn("sleep 30", &sec, PtySize::default()).expect("test: spawn sleeper in a PTY");
        session
            .resize(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("test: resize accepted");
        session.kill().await.expect("test: cleanup kill");
    }

    // ── v3b: detach keeps the PTY alive; the drain reader persists ────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn detach_keeps_pty_alive_and_attachable() {
        // The headline v3b behaviour: detaching must NOT kill the child; the
        // session stays attachable.
        let sec = auto_security();
        let session = PtyShellSession::spawn("cat", &sec, PtySize::default()).expect("test: spawn cat in a PTY");

        // Attach then detach (simulating Ctrl-]). The child must remain alive.
        let _ = session.attach();
        session.detach();
        // Give any (wrong) teardown a chance to land.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!session.has_exited(), "detach must NOT kill the PTY child");
        assert!(session.is_attachable(), "a detached live PTY must remain attachable");

        // It is still functional: a re-attach + write still echoes.
        let _ = session.attach();
        for &b in b"after-detach\n" {
            session.write_input(b).expect("test: write after re-attach");
        }
        let mut saw = false;
        for _ in 0..50 {
            if String::from_utf8_lossy(&session.attach()).contains("after-detach") {
                saw = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(saw, "PTY still echoes after a detach/re-attach cycle");

        session.kill().await.expect("test: cleanup kill");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drain_reader_keeps_filling_ring_while_detached() {
        // The drain reader must keep reading the master while detached — otherwise
        // a chatty child fills the kernel pty buffer and blocks. We spawn a child
        // that emits a burst, never attach (stay detached the whole time), and
        // assert the ring captured the output (proof the reader drained it).
        let sec = auto_security();
        let session = PtyShellSession::spawn(
            "for i in $(seq 1 200); do echo line-$i; done; sleep 5",
            &sec,
            PtySize::default(),
        )
        .expect("test: spawn chatty child");

        // Never attach. Poll the ring (snapshot peeks the buffer; we re-detach so
        // we never actually mirror to stdout in the test).
        let mut captured = String::new();
        for _ in 0..100 {
            let ring = session.attach();
            session.detach();
            captured = String::from_utf8_lossy(&ring).into_owned();
            if captured.contains("line-200") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            captured.contains("line-200"),
            "drain reader did not capture detached output — child would block: {:?}",
            &captured.get(captured.len().saturating_sub(80)..)
        );

        session.kill().await.expect("test: cleanup kill");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ring_replays_recent_context_on_attach() {
        // Re-attach replay: after the child produces output, an attach snapshot
        // returns that output (the bytes a re-attach would replay to the screen).
        let sec = auto_security();
        let session = PtyShellSession::spawn("echo replay-marker; sleep 5", &sec, PtySize::default())
            .expect("test: spawn echo child");

        let mut snapshot = Vec::new();
        for _ in 0..50 {
            snapshot = session.attach();
            session.detach();
            if String::from_utf8_lossy(&snapshot).contains("replay-marker") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            String::from_utf8_lossy(&snapshot).contains("replay-marker"),
            "attach snapshot must replay recent ring context"
        );

        session.kill().await.expect("test: cleanup kill");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_done_flag_set_when_child_exits() {
        // The drain reader observes EOF and sets child_done when the child exits on
        // its own, so the attach loop ends promptly.
        let sec = auto_security();
        let session =
            PtyShellSession::spawn("exit 0", &sec, PtySize::default()).expect("test: spawn fast-exiting child");
        let child_done = session.child_done_flag();
        let mut done = false;
        for _ in 0..100 {
            if child_done.load(Ordering::Acquire) {
                done = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            done,
            "child_done must be set after the child exits (drain reader saw EOF)"
        );
        assert!(!session.is_attachable(), "an exited PTY is not attachable");
        session.kill().await.expect("test: idempotent cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reap_reader_is_bounded_even_with_orphan_holding_slave() {
        // P0-A end-to-end: spawn a shell that backgrounds a long sleeper into its
        // OWN session (`setsid`), so killing the PTY child's group does NOT reap
        // the orphan; the orphan keeps the slave PTY open, so the master NEVER sees
        // EOF. Prove the persistent drain reader still stops (bounded) on teardown
        // via `reap_reader` — this is exactly the detach-then-kill path that must
        // never freeze the chat.
        let sec = auto_security();
        let session = PtyShellSession::spawn("(setsid sleep 300 >/dev/null 2>&1 &) ; exit", &sec, PtySize::default())
            .expect("test: spawn shell that orphans a sleeper");

        // Give the shell time to spawn the orphan and exit; the orphan now holds
        // the slave open so no EOF will arrive at the master.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        session.kill_now(); // mirror the kill path; must NOT reap the orphan

        // `reap_reader` must return within a bounded window EVEN THOUGH EOF never
        // comes (orphan holds the slave). It runs on a blocking thread so the test
        // runtime is free.
        let session_for_reap = session.clone();
        let reaped = tokio::task::spawn_blocking(move || {
            let start = std::time::Instant::now();
            session_for_reap.reap_reader();
            start.elapsed()
        })
        .await
        .expect("test: reap task joins");
        assert!(
            reaped < std::time::Duration::from_secs(5),
            "reap_reader did not return within bound — chat would freeze, took {reaped:?}"
        );

        // Best-effort cleanup of the orphan so the test box isn't littered.
        session.kill().await.ok();
    }
}
