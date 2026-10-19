//! PTY attach commands and the raw-terminal attach loop.

#[cfg(feature = "terminal-tui")]
use super::terminal::{CHAT_KEYBOARD_ENHANCEMENT_ACTIVE, CHAT_MOUSE_CAPTURE_ACTIVE};
#[cfg(feature = "terminal-tui")]
use anyhow::Result;
#[cfg(feature = "terminal-tui")]
use std::sync::Arc;
#[cfg(feature = "terminal-tui")]
use std::sync::atomic::Ordering;
#[cfg(feature = "terminal-tui")]
use tokio::sync::mpsc;

/// Handle `/pty <command>` (v3a): spawn an interactive PTY shell and hand the
/// terminal over to it for the duration of an attach.
///
/// Flow:
/// 1. Spawn the PTY session (security-gated, hardened env) at the host
///    terminal's current size.
/// 2. Register it so `/sessions` / `/kill` can see / terminate it.
/// 3. Acquire a [`PtyHandoffGuard`] — this pauses the chat render loop and
///    blocks until it has parked, so we can take over `stdin`/`stdout` without a
///    keystroke-stealing race. The guard's `Drop` restores the chat TUI on
///    **every** exit path.
/// 4. Run the byte passthrough on blocking threads until detach (`Ctrl-]`) or
///    child exit.
/// 5. The guard drops here, restoring the chat TUI (resume render loop + force a
///    full redraw to wipe PTY residue).
#[cfg(feature = "terminal-tui")]
pub(super) async fn handle_pty_command(
    command: &str,
    workspace_dir: &std::path::Path,
    chat_sessions: &mut crate::chat::sessions::ChatSessionsHandle,
    handoff: &Arc<crate::chat::sessions::pty::HandoffControl>,
    redraw_handle: Option<&mpsc::Sender<()>>,
    emit_chat_output: &impl Fn(&str),
) {
    use crate::chat::sessions::pty::PtyShellSession;
    use portable_pty::PtySize;

    // Host terminal size → PTY winsize (fall back to a sane 80x24).
    let size = crossterm::terminal::size().map_or(
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        },
        |(cols, rows)| PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        },
    );

    // v3b: enforce the live-PTY cap before spawning. Detached PTYs stay alive
    // (each holds a drain thread + ring + fds), so refuse a new one past the limit
    // with a hint to `/kill` an existing session.
    if chat_sessions.live_pty_count() >= crate::chat::sessions::pty::MAX_LIVE_PTYS {
        emit_chat_output(&format!(
            "Too many live PTY sessions (limit {}). Detach and /kill one before opening another.",
            crate::chat::sessions::pty::MAX_LIVE_PTYS
        ));
        return;
    }

    let session = match PtyShellSession::spawn(command, workspace_dir, size) {
        Ok(session) => session,
        Err(e) => {
            emit_chat_output(&format!("Failed to start interactive PTY session: {e}"));
            return;
        }
    };
    let seq = chat_sessions.add_pty(session.clone());
    emit_chat_output(&format!(
        "Interactive PTY session #{seq}: {command} — Ctrl-] to detach (Ctrl-C/Ctrl-D pass through to the shell)."
    ));

    // First `/pty` goes straight through the unified re-attach entry point, so the
    // spawn path and a later `/attach` share exactly one passthrough code path.
    reattach_pty(&session, seq, handoff, redraw_handle, emit_chat_output).await;
}

/// Re-attach (or first-attach) to a live PTY session: hand the terminal to it,
/// replay recent context, drive the stdin↔PTY passthrough, and on detach restore
/// the chat TUI **without killing the PTY** (v3b).
///
/// This is the single entry point for both `/pty <cmd>` (right after spawn) and
/// `/attach <seq>` of an already-live PTY. It reuses every v3a safety mechanism:
///
/// - [`PtyHandoffGuard::acquire`] pauses the render loop and refuses the attach
///   (returning the terminal untouched) if the loop never acks the pause; and
/// - the guard's `Drop` restores terminal modes + forces a full redraw on every
///   exit path (detach, child exit, error, panic).
///
/// Unlike v3a, the persistent drain reader and the writer live in the session's
/// runtime and survive detach: this function only *borrows* them for the attach.
/// Detach flips the sink off `stdout` (under the sink lock, so no byte races the
/// render loop's resume) and leaves the child running.
#[cfg(feature = "terminal-tui")]
pub(super) async fn reattach_pty(
    session: &crate::chat::sessions::pty::PtyShellSession,
    seq: u64,
    handoff: &Arc<crate::chat::sessions::pty::HandoffControl>,
    redraw_handle: Option<&mpsc::Sender<()>>,
    emit_chat_output: &impl Fn(&str),
) {
    use crate::chat::sessions::pty::PtyHandoffGuard;

    // Build a redraw nudge for the guard so the chat TUI repaints the instant we
    // resume (rather than waiting out the render loop's idle poll).
    let redraw_nudge: Option<Box<dyn Fn() + Send>> = redraw_handle.cloned().map(|tx| {
        let f: Box<dyn Fn() + Send> = Box::new(move || {
            let _ = tx.try_send(());
        });
        f
    });

    let handoff = Arc::clone(handoff);
    let session_for_passthrough = session.clone();
    let outcome = crate::runtime::blocking::spawn_blocking(move || {
        // Acquire the handoff guard: pause the render loop and wait for its ack. If
        // the ack times out we do NOT proceed (running while the render loop might
        // still touch the terminal would corrupt the screen). `acquire` un-pauses
        // the render loop itself on timeout, so we just report the abort.
        let Some(_guard) = PtyHandoffGuard::acquire(
            handoff,
            redraw_nudge,
            CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.load(std::sync::atomic::Ordering::Acquire),
            CHAT_MOUSE_CAPTURE_ACTIVE.load(std::sync::atomic::Ordering::Acquire),
        ) else {
            return PtyOutcome::AttachAborted;
        };
        PtyOutcome::Exited(run_pty_attach(&session_for_passthrough))
        // `_guard` drops here → terminal modes restored, render loop resumes +
        // full redraw forced. The PTY child stays alive (no kill on detach).
    })
    .await;

    match outcome {
        Ok(PtyOutcome::Exited(PtyExit::Detached)) => {
            emit_chat_output(&format!(
                "Detached from PTY session #{seq} (still running — /attach #{seq} to return, /kill #{seq} to stop)."
            ));
        }
        Ok(PtyOutcome::Exited(PtyExit::ChildExited)) => {
            // The child exited; reap the drain thread so it does not linger.
            session.reap_reader();
            emit_chat_output(&format!("Interactive PTY session #{seq} exited."));
        }
        Ok(PtyOutcome::AttachAborted) => {
            // The render loop never acked the pause; we refused the handoff to
            // avoid two threads fighting over the terminal. The PTY is untouched
            // and still attachable later.
            tracing::warn!(seq, "PTY attach aborted: render loop did not park in time");
            emit_chat_output(&format!(
                "Could not enter PTY session #{seq}: the chat renderer did not pause in time (terminal unchanged)."
            ));
        }
        Err(e) => {
            // The passthrough task panicked; the guard's Drop still ran during
            // unwind, so the terminal is restored. The session detaches defensively
            // (it stays alive for a later attempt). Surface the fault.
            session.detach();
            tracing::error!(error = %e, seq, "PTY passthrough task panicked");
            emit_chat_output(&format!("PTY session #{seq} ended unexpectedly; terminal restored."));
        }
    }
}

/// The result of an attempted `/pty` attach: either the passthrough ran and
/// ended ([`PtyExit`]), or the handoff was refused because the render loop never
/// acknowledged the pause (so the terminal was left untouched).
#[cfg(feature = "terminal-tui")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtyOutcome {
    /// The passthrough ran and ended this way.
    Exited(PtyExit),
    /// The handoff was aborted before takeover (render loop did not park).
    AttachAborted,
}

/// How an interactive PTY passthrough ended.
#[cfg(feature = "terminal-tui")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtyExit {
    /// The user pressed `Ctrl-]`.
    Detached,
    /// The PTY child process exited on its own.
    ChildExited,
}

/// Drive the raw terminal ⇄ PTY byte passthrough for an attach until detach or
/// child exit (v3b).
///
/// Runs on a `spawn_blocking` thread with the chat render loop parked (the
/// [`PtyHandoffGuard`] owned by the caller guarantees that). Unlike v3a it does
/// **not** spawn a per-attach reader or own the writer: the session's persistent
/// drain reader is already running, and this function only:
///
/// 1. syncs the PTY + emulator to the current host size, clears the screen, and
///    renders the in-process emulator's current screen so the user sees the exact
///    on-screen state (re-attach restore, v3b-b — correct for vim/htop, not just a
///    raw byte replay);
/// 2. marks the sink attached so the drain reader mirrors live PTY output to
///    `stdout` while we are here;
/// 3. nudges the child with a `SIGWINCH` size jitter as a secondary safeguard so a
///    program tracking its own size also re-flows (v3b-b);
/// 4. reads raw `stdin`, classifying each byte
///    ([`crate::chat::sessions::pty::classify_input_byte`]) — `Ctrl-]` detaches,
///    everything else (incl. `Ctrl-C`/`Ctrl-D`) is forwarded to the PTY child —
///    and rechecks the child-done flag each tick so a child that exits while the
///    user is idle ends the attach promptly.
///
/// On **every** exit path (detach, child exit, error, panic) the local RAII
/// `AttachScope` detaches the sink — under the sink lock, so the drain reader is
/// guaranteed to have stopped writing `stdout` before the `PtyHandoffGuard`
/// resumes the chat render loop (the v3a invariant). The child is **not** killed:
/// the PTY stays alive for a later re-attach. Never panics: I/O errors end the
/// attach and the guard still restores the terminal.
#[cfg(feature = "terminal-tui")]
fn run_pty_attach(session: &crate::chat::sessions::pty::PtyShellSession) -> PtyExit {
    use crate::chat::sessions::pty::{InputByte, classify_input_byte};
    use std::io::Write as _;

    // Current host geometry (for resize-forward seed + redraw nudge). Fall back to
    // a sane 80x24 if crossterm cannot read it.
    let host_size = || {
        crossterm::terminal::size().map_or(
            portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            |(cols, rows)| portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
    };

    // RAII: detach the sink on EVERY exit path (incl. panic unwind) so the drain
    // reader stops mirroring to `stdout` before the chat render loop resumes.
    struct AttachScope<'a> {
        session: &'a crate::chat::sessions::pty::PtyShellSession,
    }
    impl Drop for AttachScope<'_> {
        fn drop(&mut self) {
            // Under the sink lock: after this returns the reader will not write
            // `stdout` again until re-attached (v3a invariant). Does NOT kill the
            // child — the PTY survives detach for re-attach.
            self.session.detach();
        }
    }
    let _scope = AttachScope { session };

    // 0. Sync the PTY + emulator to the CURRENT host geometry BEFORE rendering the
    //    restore. The host may have been resized while this PTY was detached; if we
    //    rendered the emulator's old-size screen it would be offset / wrapped wrong.
    //    `resize` updates the emulator grid and the PTY master together (v3b-b), so
    //    the subsequent `attach()` redraw is laid out for the real terminal. Cheap,
    //    synchronous, non-fatal.
    if let Err(e) = session.resize(host_size()) {
        tracing::debug!(error = %e, "PTY resize-to-host before re-attach redraw failed");
    }

    // 1. Clear the screen + home the cursor, then attach. `attach()` flips the sink
    //    to `attached` AND renders the emulator's CURRENT screen to `stdout`
    //    atomically under the sink lock (v3b-b): because the drain reader's
    //    live-mirror `write()` (which also feeds the emulator) contends for that
    //    same lock, the screen-restore escape codes can never interleave with live
    //    bytes — the restore completes first, then live bytes follow in order.
    //    Rendering from the emulator (`state_formatted`) rather than replaying the
    //    raw ring means full-screen programs (vim/htop) re-appear correct on the
    //    first frame instead of being scrambled by spliced-in cursor sequences.
    {
        let mut out = std::io::stdout();
        let _ = out.write_all(b"\x1b[2J\x1b[H");
        let _ = out.flush();
    }
    session.attach();

    // 3. Secondary safeguard (v3b-b): the emulator already restored the picture, but
    //    nudge the child with a SIGWINCH size jitter so a program tracking its own
    //    size also re-flows. Harmless to streaming programs.
    session.nudge_redraw(host_size());

    // ── This thread: stdin → PTY writer, watching for detach + child exit ─────
    //
    // The session's drain reader owns child-done detection; we borrow its flag so
    // the stdin loop ends promptly when the child exits while the user is idle.
    let child_done = session.child_done_flag();

    // v5: forward host terminal resizes to the PTY. The chat render loop is parked
    // during the handoff, so crossterm `Resize` events go unconsumed; instead we
    // poll the host size each loop tick (≤100 ms) and push a `resize` to the PTY
    // master whenever it changes, so full-screen curses programs (vim, htop, …)
    // re-flow. `resize` is cheap, synchronous and non-panicking. We seed
    // `last_size` from the geometry we just nudged to so the first real change is
    // detected.
    let mut last_size: Option<(u16, u16)> = crossterm::terminal::size().ok();
    let on_tick = || {
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            if last_size != Some((cols, rows)) {
                last_size = Some((cols, rows));
                if let Err(e) = session.resize(portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    tracing::warn!(error = %e, cols, rows, "PTY resize forward failed");
                }
            }
        }
    };
    let result = pty_stdin_loop(&child_done, on_tick, |byte| {
        match classify_input_byte(byte) {
            InputByte::Detach => Ok(true), // stop the loop (detach)
            InputByte::Forward => {
                // Forward to the session writer. If the writer is gone the child
                // has exited / been reaped — treat as child exit (stop the loop)
                // rather than erroring, so detach stays clean.
                if session.write_input(byte).is_err() {
                    return Ok(true);
                }
                Ok(false)
            }
        }
    });

    // `_scope` drops here (detach sink) regardless of how the loop ended. Map any
    // stdin-loop error to a child-exit-equivalent terminal outcome — the PTY is
    // left alive and re-attachable; the guard restores the terminal.
    match result {
        Ok(exit) => exit,
        Err(e) => {
            tracing::warn!(error = %e, "PTY attach stdin loop ended with error; detaching");
            PtyExit::Detached
        }
    }
}

/// Read raw `stdin` byte-by-byte, invoking `on_byte` for each, until `on_byte`
/// returns `Ok(true)` (detach) or `child_done` is observed (child exit).
///
/// Uses `libc::poll` on fd 0 with a 100 ms timeout so a child that exits while
/// the user is idle ends the passthrough promptly (a blocking `read` alone would
/// hang until the next keystroke). On non-Unix targets, where this `poll` is
/// unavailable, it falls back to a plain blocking read (the child-exit-while-idle
/// case is handled when the reader thread closes `stdin`'s peer; documented
/// platform limitation, plan §v3 risk 3).
///
/// `on_byte` returns `Ok(true)` to stop (detach), `Ok(false)` to continue, or an
/// `Err` to abort the passthrough (surfaced to the caller; the guard still
/// restores the terminal).
#[cfg(feature = "terminal-tui")]
#[allow(unsafe_code)]
fn pty_stdin_loop(
    child_done: &Arc<std::sync::atomic::AtomicBool>,
    mut on_tick: impl FnMut(),
    mut on_byte: impl FnMut(u8) -> Result<bool>,
) -> Result<PtyExit> {
    use std::io::Read as _;

    let mut stdin = std::io::stdin();
    let mut buf = [0u8; 1024];

    loop {
        if child_done.load(Ordering::Acquire) {
            return Ok(PtyExit::ChildExited);
        }

        // Per-iteration housekeeping that must run even while the user is idle
        // (the poll below wakes at least every 100 ms). Used to forward host
        // terminal resizes to the PTY so curses programs re-flow. The render
        // loop is parked during the handoff, so crossterm `Resize` events are
        // not being consumed elsewhere — polling the size here is the
        // self-contained way to track it.
        on_tick();

        // Wait (bounded) for stdin to be readable so we can re-check child exit.
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            let mut pfd = libc::pollfd {
                fd: stdin.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `poll` reads/writes exactly the one `pollfd` we pass
            // (`nfds = 1`); the pointer is to a live local, valid for the call.
            // It dereferences nothing else and has no memory-safety
            // preconditions. A 100 ms timeout bounds the wait.
            let rc = unsafe { libc::poll(&raw mut pfd, 1, 100) };
            if rc <= 0 {
                // Timeout (0) or EINTR (<0): loop to re-check `child_done`.
                continue;
            }
            if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 && pfd.revents & libc::POLLIN == 0 {
                // stdin hung up with no data: treat as child-exit-equivalent.
                return Ok(PtyExit::ChildExited);
            }
        }

        let n = match stdin.read(&mut buf) {
            Ok(0) => return Ok(PtyExit::ChildExited), // stdin EOF
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(anyhow::anyhow!("PTY stdin read failed: {e}")),
        };
        for &byte in buf.get(..n).unwrap_or(&buf) {
            if on_byte(byte)? {
                return Ok(PtyExit::Detached);
            }
        }
    }
}
