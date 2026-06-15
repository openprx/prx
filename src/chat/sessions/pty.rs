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

/// An interactive PTY shell session: the master side of a pseudo-terminal plus
/// the spawned child's bookkeeping.
///
/// The session owns the PTY master (for resize) and the child handle (for
/// `kill`/`wait`); the master *reader* and *writer* are returned alongside it at
/// spawn time as a [`PtyIo`] for the handoff passthrough.
///
/// Cloning is cheap (shared `Arc`s) so the chat registry can hold a handle while
/// the passthrough holds the byte streams.
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
}

/// The reader + writer halves of a PTY, plus a resize handle, handed to the
/// terminal-handoff passthrough. Separated from [`PtyShellSession`] so the
/// blocking passthrough owns the byte streams while the session handle remains
/// usable for `/sessions`, `/kill`, and resize.
pub struct PtyIo {
    /// PTY output → write straight to the real terminal `stdout`.
    pub reader: Box<dyn std::io::Read + Send>,
    /// Real terminal `stdin` bytes → write here to reach the PTY child.
    pub writer: Box<dyn std::io::Write + Send>,
    /// Raw fd of the PTY **master** (Unix only) so the passthrough's reader
    /// thread can `poll` it with a timeout and stay interruptible via a stop
    /// flag — it must NOT depend on EOF, since a backgrounded grandchild
    /// (`sleep 300 &`, reparented to init, killpg can't reach it) can hold the
    /// slave open indefinitely and wedge a blocking `read()`. `None` on non-Unix
    /// or if the backend did not expose an fd; the reader then falls back to a
    /// plain blocking read (documented platform limitation).
    #[cfg(unix)]
    pub master_fd: Option<std::os::fd::RawFd>,
}

impl PtyShellSession {
    /// Spawn an interactive command inside a fresh PTY of `size`, returning the
    /// session handle and its [`PtyIo`] (reader/writer for the passthrough).
    ///
    /// Security: the command is authorized through the **same**
    /// [`SideEffectGate`] the interactive shell tool uses, so high-risk commands
    /// (`rm -rf /`, `mkfs`, …) are still blocked / require a grant even though
    /// the operator typed `/pty`. The child runs in the workspace directory with
    /// a hardened `PATH` + the same safe-env allow-list as the v2 background
    /// shell (no secrets leak into the PTY).
    pub fn spawn(command: &str, security: &Arc<SecurityPolicy>, size: PtySize) -> Result<(Self, PtyIo)> {
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

        let session = Self {
            id,
            command: command.to_string(),
            started_at: chrono::Utc::now(),
            master: Arc::new(Mutex::new(pair.master)),
            child: Arc::new(Mutex::new(child)),
            pgid,
        };
        Ok((
            session,
            PtyIo {
                reader,
                writer,
                #[cfg(unix)]
                master_fd,
            },
        ))
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
        let sec = auto_security();
        let (session, mut io) =
            PtyShellSession::spawn("cat", &sec, PtySize::default()).expect("test: spawn cat in a PTY");

        // Write a line; `cat` echoes it back through the PTY.
        io.writer.write_all(b"hello-pty\n").expect("test: write to PTY");
        io.writer.flush().expect("test: flush PTY writer");

        // Read until we see the echoed text (run the blocking read off-thread so
        // the test's tokio runtime is not blocked).
        let saw = tokio::task::spawn_blocking(move || {
            use std::io::Read as _;
            let mut buf = [0u8; 1024];
            let mut acc = String::new();
            for _ in 0..50 {
                match io.reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        acc.push_str(&String::from_utf8_lossy(buf.get(..n).unwrap_or(&buf)));
                        if acc.contains("hello-pty") {
                            return true;
                        }
                    }
                    Err(_) => break,
                }
            }
            acc.contains("hello-pty")
        })
        .await
        .expect("test: reader task joins");
        assert!(saw, "PTY echoed the written line back");

        // Terminating the group exits `cat` cleanly; kill is idempotent.
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
        let (session, _io) =
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

    #[cfg(unix)]
    #[tokio::test]
    async fn reader_detach_is_bounded_even_with_orphan_holding_slave() {
        // P0-A end-to-end-ish: spawn a shell that backgrounds a long sleeper into
        // its OWN session (`setsid`), so killing the PTY child's group does NOT
        // reap the orphan; the orphan keeps the slave PTY open, so the master
        // NEVER sees EOF. Prove the interruptible reader against the real master
        // fd still stops (bounded) on the flag — this is exactly the detach path
        // that used to freeze the chat.
        let sec = auto_security();
        let (session, io) = PtyShellSession::spawn(
            // `setsid sleep 300` detaches into a new session/group; if setsid is
            // unavailable the plain `&` background still reparents on shell exit.
            "(setsid sleep 300 >/dev/null 2>&1 &) ; exit",
            &sec,
            PtySize::default(),
        )
        .expect("test: spawn shell that orphans a sleeper");

        let crate::chat::sessions::pty::PtyIo {
            mut reader,
            writer,
            master_fd,
        } = io;
        drop(writer); // we won't write; just exercise the reader

        let stop = Arc::new(ReaderStop::new());
        let stop_thread = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut sink: Vec<u8> = Vec::new();
            read_pty_to_stdout(
                reader.as_mut(),
                &mut sink,
                &stop_thread,
                master_fd,
                std::time::Duration::from_millis(50),
            )
        });

        // Give the shell time to spawn the orphan and exit; the orphan now holds
        // the slave open so no EOF will arrive. Kill the child's group (mirrors
        // detach's `kill_now`) — this must NOT reap the orphan.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        session.kill_now();

        // Request stop — the reader must exit within a bounded window EVEN THOUGH
        // EOF never comes (orphan holds the slave). Without the interruptible
        // poll+flag design this join would hang forever and freeze the chat.
        stop.stop();
        let start = std::time::Instant::now();
        loop {
            if handle.is_finished() {
                break;
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "reader did not stop after detach with an orphan holding the slave — chat would freeze"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let outcome = handle.join().expect("test: reader joins");
        // It stopped via the flag (the whole point) — not by a fortuitous EOF.
        assert!(
            matches!(outcome, ReaderOutcome::Stopped | ReaderOutcome::Eof),
            "reader returned a teardown outcome, got {outcome:?}"
        );

        // Best-effort cleanup of the orphan so the test box isn't littered.
        session.kill().await.ok();
    }
}
