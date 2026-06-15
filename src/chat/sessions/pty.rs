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
    /// `redraw_nudge`, if supplied, is invoked on `Drop` to wake the render loop
    /// immediately (e.g. a `move || { let _ = redraw_tx.try_send(()); }`).
    pub fn acquire(control: Arc<HandoffControl>, redraw_nudge: Option<Box<dyn Fn() + Send>>) -> Self {
        // Block (bounded) until the render loop confirms it has parked. We
        // deliberately ignore the bool: on timeout we proceed anyway (the
        // handoff is best-effort and the guard's Drop still restores), never
        // deadlocking.
        let _acked = control.pause_and_wait(PAUSE_ACK_TIMEOUT);
        Self { control, redraw_nudge }
    }
}

impl Drop for PtyHandoffGuard {
    fn drop(&mut self) {
        // 1. Resume the render loop + force a full redraw to clear PTY residue.
        self.control.resume_with_redraw();
        // 2. Defensively re-assert the chat terminal modes the PTY child may
        //    have clobbered. Best-effort — we are on the restoration path and
        //    have no caller to surface errors to; the render loop's own
        //    `TerminalGuard` is the primary owner of these.
        let _ = crossterm::terminal::enable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste);
        // 3. Nudge the render loop to repaint right away.
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
        Ok((session, PtyIo { reader, writer }))
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
                // SAFETY: `killpg` is an async-signal-safe libc call that only
                // signals the process group `pgid`; it dereferences no pointers
                // and has no memory-safety preconditions. `pgid` is the session
                // leader pid of our own PTY child (a descendant group), so
                // signalling it is sound.
                unsafe {
                    // SIGHUP first (the natural "terminal closed" signal for an
                    // interactive shell), then SIGKILL to guarantee teardown.
                    libc::killpg(pgid, libc::SIGHUP);
                    libc::killpg(pgid, libc::SIGKILL);
                }
                return;
            }
        }
        // No pgid: best-effort kill of the direct child.
        let _ = self.child.lock().kill();
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
    // representation used by killpg.
    pid.and_then(|pid| i32::try_from(pid).ok())
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

    #[test]
    fn guard_drop_resumes_and_forces_redraw() {
        let control = Arc::new(HandoffControl::new());
        let nudged = Arc::new(AtomicBool::new(false));
        let nudged2 = Arc::clone(&nudged);
        {
            let _guard = PtyHandoffGuard::acquire(
                Arc::clone(&control),
                Some(Box::new(move || nudged2.store(true, Ordering::Release))),
            );
            // During the handoff the render loop is paused.
            assert!(control.is_paused());
        }
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
        let control2 = Arc::clone(&control);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = PtyHandoffGuard::acquire(Arc::clone(&control2), None);
            assert!(control2.is_paused());
            panic!("test: simulate a fault during PTY handoff");
        }));
        assert!(result.is_err(), "the closure panicked");
        assert!(
            !control.is_paused(),
            "terminal must be restored (render loop resumed) even after a panic"
        );
        assert!(control.take_force_redraw());
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
}
