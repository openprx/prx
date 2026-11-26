use super::*;

fn auto_security() -> PathBuf {
    std::env::temp_dir()
}

fn read_only_security() -> PathBuf {
    std::env::temp_dir()
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
            true,
            true,
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
        let _guard = PtyHandoffGuard::acquire(Arc::clone(&control2), None, true, true)
            .expect("test: ack arrives so acquire succeeds");
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
fn fullscreen_guard_restores_even_on_panic_unwind() {
    // Drop writes the child-mode reset and re-enters chat's alt-screen before
    // unpausing the renderer.
    let control = Arc::new(HandoffControl::new());
    let acker = spawn_acking_render_loop(&control);
    let control2 = Arc::clone(&control);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = PtyHandoffGuard::acquire(Arc::clone(&control2), None, true, true)
            .expect("test: ack arrives so acquire succeeds");
        assert!(control2.is_paused());
        panic!("test: simulate a fullscreen PTY handoff fault");
    }));
    acker.join().expect("test: acker joins");
    assert!(result.is_err(), "the closure panicked");
    assert!(
        !control.is_paused(),
        "fullscreen terminal handoff must resume the render loop after panic"
    );
    assert!(control.take_force_redraw(), "fullscreen panic path must force redraw");
}

#[test]
fn acquire_aborts_when_render_loop_never_parks() {
    // P0-D: if the render loop never acks the pause, `acquire` must return
    // `None` and leave the control un-paused (handoff refused, terminal
    // untouched), never proceed with a half-done handoff.
    let control = Arc::new(HandoffControl::new());
    // No acker thread → the ack will time out.
    let guard = PtyHandoffGuard::acquire(Arc::clone(&control), None, true, true);
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
    let guard = PtyHandoffGuard::acquire(Arc::clone(&control), None, true, true)
        .expect("test: ack arrives so acquire succeeds");
    assert!(control.is_paused());
    drop(guard);
    acker.join().expect("test: acker joins");
    // After drop: unpaused AND force_redraw armed (so the render loop repaints
    // the chrome rather than leaving PTY residue / a blank viewport).
    assert!(!control.is_paused(), "guard drop must unpause");
    assert!(control.take_force_redraw(), "force_redraw must be set on resume");
}

// ── v3b-b review P1: host terminal mode reset on detach ──────────────────

#[test]
fn host_mode_reset_emits_all_baseline_sequences() {
    // The detach reset must turn OFF every mode a PTY child (vim/htop/less)
    // could have left the terminal in: mouse tracking, application cursor
    // (DECCKM), application keypad, hidden cursor, and the alternate screen.
    // We assert each specific escape sequence is present in what gets written,
    // so a regression that drops one (re-introducing the residual-mode bug)
    // fails loudly.
    let mut captured: Vec<u8> = Vec::new();
    write_handoff_terminal_restore(&mut captured, true, true);
    let s = String::from_utf8(captured).expect("test: reset is valid utf-8");

    // Mouse tracking off (X10 / button / any-motion / utf8 / sgr coords).
    for seq in [
        "\x1b[?1006l",
        "\x1b[?1015l",
        "\x1b[?1005l",
        "\x1b[?1003l",
        "\x1b[?1002l",
        "\x1b[?1000l",
    ] {
        assert!(s.contains(seq), "host mode reset missing mouse-off seq {seq:?}: {s:?}");
    }
    // Normal cursor keys (DECCKM off).
    assert!(s.contains("\x1b[?1l"), "host mode reset missing DECCKM-off: {s:?}");
    // Numeric keypad (DECKPNM), undoing application keypad.
    assert!(s.contains("\x1b>"), "host mode reset missing keypad-numeric: {s:?}");
    // Cursor visible.
    assert!(s.contains("\x1b[?25h"), "host mode reset missing show-cursor: {s:?}");
    // Leave alternate screen (modern + legacy).
    assert!(
        s.contains("\x1b[?1049l"),
        "host mode reset missing alt-screen-off (1049): {s:?}"
    );
    assert!(
        s.contains("\x1b[?47l"),
        "host mode reset missing alt-screen-off (47): {s:?}"
    );
}

#[test]
fn host_mode_reset_is_idempotent_constant() {
    // Re-sending is harmless: the function must always emit the SAME bytes
    // (these are chat's baseline modes, sent on every detach regardless of
    // whether a child actually changed them, including the no-TUI path).
    let mut a: Vec<u8> = Vec::new();
    let mut b: Vec<u8> = Vec::new();
    write_handoff_terminal_restore(&mut a, true, true);
    write_handoff_terminal_restore(&mut b, true, true);
    assert_eq!(a, b, "host mode reset must be a deterministic constant");
    assert!(
        a.starts_with(HOST_MODE_RESET) && a.ends_with(CHAT_KEYBOARD_ENHANCEMENT_PUSH),
        "writer must reset child modes, re-enter chat alt-screen, re-enable mouse, and re-push keyboard flags"
    );
}

#[test]
fn fullscreen_handoff_restore_resets_child_then_reenters_chat_alt_screen_and_mouse() {
    let mut captured: Vec<u8> = Vec::new();
    write_handoff_terminal_restore(&mut captured, true, true);
    assert!(
        captured.starts_with(HOST_MODE_RESET),
        "fullscreen restore must first reset child terminal modes: {captured:?}"
    );
    let alt_enter = captured
        .windows(CHAT_ALT_SCREEN_ENTER.len())
        .position(|window| window == CHAT_ALT_SCREEN_ENTER)
        .expect("test: restore should re-enter chat alternate screen");
    let mouse_enable = captured
        .windows(CHAT_MOUSE_CAPTURE_ENABLE.len())
        .position(|window| window == CHAT_MOUSE_CAPTURE_ENABLE)
        .expect("test: restore should re-enable chat mouse capture");
    assert!(
        alt_enter < mouse_enable,
        "fullscreen restore must re-enable chat mouse capture after alt-screen enter: {captured:?}"
    );
    assert!(
        captured.ends_with(CHAT_KEYBOARD_ENHANCEMENT_PUSH),
        "fullscreen restore must re-push keyboard enhancement flags after handoff: {captured:?}"
    );
}

#[test]
fn fullscreen_handoff_restore_does_not_enable_mouse_when_chat_mouse_was_off() {
    let mut captured: Vec<u8> = Vec::new();
    write_handoff_terminal_restore(&mut captured, true, false);

    assert!(
        captured.starts_with(HOST_MODE_RESET),
        "fullscreen restore must still reset child terminal modes: {captured:?}"
    );
    assert!(
        captured
            .windows(CHAT_ALT_SCREEN_ENTER.len())
            .any(|window| window == CHAT_ALT_SCREEN_ENTER),
        "fullscreen restore must still re-enter chat alt-screen: {captured:?}"
    );
    assert!(
        !captured
            .windows(CHAT_MOUSE_CAPTURE_ENABLE.len())
            .any(|window| window == CHAT_MOUSE_CAPTURE_ENABLE),
        "mouse-off chat sessions must not re-enable mouse capture after PTY detach: {captured:?}"
    );
}

#[test]
fn fullscreen_attach_leave_disables_chat_mouse_before_alt_leave() {
    let mut captured: Vec<u8> = Vec::new();
    write_chat_alt_screen_leave_for_handoff(&mut captured, true);
    assert!(
        captured.starts_with(CHAT_KEYBOARD_ENHANCEMENT_POP),
        "fullscreen attach must pop keyboard enhancement flags before child owns terminal: {captured:?}"
    );
    let keyboard_pop = captured
        .windows(CHAT_KEYBOARD_ENHANCEMENT_POP.len())
        .position(|window| window == CHAT_KEYBOARD_ENHANCEMENT_POP)
        .expect("test: keyboard enhancement pop should be emitted");
    let mouse_disable = captured
        .windows(CHAT_MOUSE_CAPTURE_DISABLE.len())
        .position(|window| window == CHAT_MOUSE_CAPTURE_DISABLE)
        .expect("test: mouse disable should be emitted");
    assert!(
        keyboard_pop < mouse_disable,
        "fullscreen attach must pop keyboard flags before disabling mouse: {captured:?}"
    );
    assert!(
        captured.ends_with(CHAT_ALT_SCREEN_LEAVE),
        "fullscreen attach must leave chat alt-screen after disabling chat mouse capture: {captured:?}"
    );
}

#[test]
fn handoff_keyboard_sequences_are_conditional() {
    let mut leave = Vec::new();
    write_chat_alt_screen_leave_for_handoff(&mut leave, false);
    assert!(
        !leave
            .windows(CHAT_KEYBOARD_ENHANCEMENT_POP.len())
            .any(|window| window == CHAT_KEYBOARD_ENHANCEMENT_POP),
        "inactive keyboard enhancement must not emit pop: {leave:?}"
    );

    let mut restore = Vec::new();
    write_handoff_terminal_restore(&mut restore, false, true);
    assert!(
        !restore
            .windows(CHAT_KEYBOARD_ENHANCEMENT_PUSH.len())
            .any(|window| window == CHAT_KEYBOARD_ENHANCEMENT_PUSH),
        "inactive keyboard enhancement must not emit push: {restore:?}"
    );
}

#[test]
fn host_mode_reset_write_error_does_not_panic() {
    // The reset is on the infallible restoration path: a failing sink must be
    // logged, never panic (iron law: never panic on the detach/Drop path).
    struct FailingSink;
    impl std::io::Write for FailingSink {
        fn write(&mut self, _b: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
    }
    let mut sink = FailingSink;
    // Must return normally (no panic) despite the write error.
    write_handoff_terminal_restore(&mut sink, true, true);
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
    assert!(out.is_empty(), "expected an empty value");
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

// ── v3b-a blocker-2: bounded join must never hang on a stuck reader ───────

#[test]
fn bounded_join_returns_true_when_thread_finishes_in_time() {
    let handle = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(20));
    });
    let start = std::time::Instant::now();
    let joined = bounded_join(handle, std::time::Duration::from_millis(500));
    assert!(joined, "thread that finishes in time must be joined");
    assert!(
        start.elapsed() < std::time::Duration::from_millis(400),
        "bounded_join should return shortly after the thread finishes"
    );
}

#[test]
fn bounded_join_gives_up_within_deadline_on_stuck_thread() {
    // A thread that ignores any stop request and parks well past the deadline,
    // modelling a stuck drain reader. `bounded_join` MUST return within the
    // deadline (plus a small slop) instead of blocking until the thread ends.
    let release = Arc::new(AtomicBool::new(false));
    let release_thread = Arc::clone(&release);
    let handle = std::thread::spawn(move || {
        // Park until released (or a hard ceiling) — far longer than the join
        // deadline below, so the join is forced to give up.
        for _ in 0..400 {
            if release_thread.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });
    let deadline = std::time::Duration::from_millis(150);
    let start = std::time::Instant::now();
    let joined = bounded_join(handle, deadline);
    let elapsed = start.elapsed();
    // Let the parked thread exit so the test process does not linger on it.
    release.store(true, Ordering::Release);
    assert!(!joined, "a stuck thread must NOT report as joined");
    assert!(
        elapsed < deadline + std::time::Duration::from_millis(150),
        "bounded_join must give up within ~deadline (took {elapsed:?}, deadline {deadline:?}) — \
             otherwise /kill / chat-exit could freeze"
    );
}

// ── v3b-b: in-process terminal emulator (feed / render / resize / serialize) ─

fn test_size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// A `Write` sink that records everything written to it and can be told to
/// sleep at the *start* of its first write — used to widen the window in which
/// a concurrent live-mirror `write()` could (wrongly) interleave, so the test
/// reliably catches a missing lock instead of relying on luck.
#[derive(Clone)]
struct RecordingSink {
    buf: Arc<Mutex<Vec<u8>>>,
    first_write_delay: std::time::Duration,
    first_write_done: Arc<AtomicBool>,
}
impl std::io::Write for RecordingSink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if !self.first_write_done.swap(true, Ordering::AcqRel) && !self.first_write_delay.is_zero() {
            std::thread::sleep(self.first_write_delay);
        }
        self.buf.lock().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn emulator_tracks_screen_after_feed() {
    // Feeding the sink (as the drain reader does) must update the emulator
    // screen, so a re-attach can render the current contents. Detached or not,
    // the screen reflects the latest bytes.
    use std::io::Write as _;
    let sink = PtySink::new(test_size(24, 80));
    {
        let mut s = sink.clone();
        s.write_all(b"hello-emulator").expect("test: feed emulator");
    }
    let screen = sink.screen_contents_for_test();
    assert!(
        screen.contains("hello-emulator"),
        "emulator screen must reflect fed bytes, got: {screen:?}"
    );
    // Overwriting the line (carriage-return + new text) must be reflected too,
    // proving the emulator interprets control bytes rather than just buffering.
    {
        let mut s = sink.clone();
        s.write_all(b"\rgoodbye").expect("test: overwrite line");
    }
    let screen = sink.screen_contents_for_test();
    assert!(
        screen.contains("goodbye"),
        "emulator must apply CR overwrite, got: {screen:?}"
    );
}

#[test]
fn attach_render_produces_nonempty_redraw_reflecting_screen() {
    // The re-attach redraw must be non-empty and reproduce the current screen.
    // We feed some text, render to a capture, then parse that render back into a
    // FRESH emulator and assert the reconstructed screen matches — proving the
    // render bytes actually rebuild the picture (not just that they are present).
    use std::io::Write as _;
    let sink = PtySink::new(test_size(10, 40));
    {
        let mut s = sink.clone();
        s.write_all(b"RESTORE-ME-XYZ").expect("test: feed emulator");
    }
    let mut captured: Vec<u8> = Vec::new();
    sink.attach_and_render_to(&mut captured);
    assert!(!captured.is_empty(), "re-attach redraw must not be empty");

    // Replay the redraw into a clean emulator of the same size; its screen must
    // show the same text we fed — i.e. the render reconstructs the screen.
    let mut fresh = vt100::Parser::new(10, 40, 0);
    fresh.process(&captured);
    let reconstructed = fresh.screen().contents();
    assert!(
        reconstructed.contains("RESTORE-ME-XYZ"),
        "re-attach redraw did not reconstruct the screen, got: {reconstructed:?}"
    );
}

#[test]
fn set_size_resizes_emulator_grid() {
    // Resizing the sink must resize the emulator grid so the redraw is laid out
    // for the real terminal geometry (otherwise a re-attach would be offset).
    let sink = PtySink::new(test_size(24, 80));
    sink.set_size(40, 120);
    // Inspect via the locked screen size.
    let (rows, cols) = sink.0.lock().parser.screen().size();
    assert_eq!((rows, cols), (40, 120), "emulator grid must follow set_size");
    // Zero dimensions are clamped to 1 (never a 0-sized grid).
    sink.set_size(0, 0);
    let (rows, cols) = sink.0.lock().parser.screen().size();
    assert_eq!((rows, cols), (1, 1), "set_size must clamp 0 to 1");
}

#[test]
fn attach_render_serializes_under_lock_no_interleave() {
    // v3b-b invariant (carried over from v3b-a blocker-1): the re-attach redraw
    // happens under the sink lock, so the drain reader's live-mirror `write()`
    // (same lock) cannot interleave a live byte into the screen-restore escape
    // codes. We race a deliberately-slow redraw against a live write and assert
    // the live write is BLOCKED until the redraw's lock hold ends (timing
    // proof), and that the live bytes still reach the emulator/ring afterwards.
    use std::io::Write as _;

    let sink = PtySink::new(test_size(24, 80));
    // Seed the screen so the redraw is non-trivial.
    {
        let mut s = sink.clone();
        s.write_all(b"SEED-SCREEN").expect("test: seed screen");
    }

    let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut recorder = RecordingSink {
        buf: Arc::clone(&captured),
        // Sleep mid-render so a live write, if it could interleave, would.
        first_write_delay: std::time::Duration::from_millis(150),
        first_write_done: Arc::new(AtomicBool::new(false)),
    };

    // Thread 1: the attach — renders the emulator screen to the recorder under
    // the sink lock (holds the lock across the slow first write).
    let sink_attach = sink.clone();
    let attach_thread = std::thread::spawn(move || {
        sink_attach.attach_and_render_to(&mut recorder);
    });

    // Give the attach a moment to acquire the lock and begin its (slow) render.
    std::thread::sleep(std::time::Duration::from_millis(30));

    // Thread 2 (this thread): a live write contends for the SAME sink lock held
    // by the in-flight render; it must block until the render's 150ms sleep ends.
    let live_marker = b"LIVE-AFTER";
    let start = std::time::Instant::now();
    {
        let mut s = sink.clone();
        s.write_all(live_marker).expect("test: live write");
    }
    let live_write_elapsed = start.elapsed();

    attach_thread.join().expect("test: attach thread joins");

    // 1. The live write was blocked by the render's lock hold: it could not
    //    return until the 150ms render sleep (started ~30ms before) completed.
    assert!(
        live_write_elapsed >= std::time::Duration::from_millis(90),
        "live mirror write returned too fast ({live_write_elapsed:?}) — it was NOT \
             serialized behind the under-lock render (interleave possible)"
    );

    // 2. The render captured a non-empty self-contained redraw.
    let got = captured.lock().clone();
    assert!(!got.is_empty(), "render produced no redraw bytes");

    // 3. The live bytes made it into the emulator AFTER the render (no loss).
    let screen = sink.screen_contents_for_test();
    assert!(
        screen.contains("LIVE-AFTER"),
        "live bytes lost from emulator after render: {screen:?}"
    );
    // And into the raw ring fallback too.
    let ring = sink.attach_and_snapshot_vec_for_test();
    assert!(
        String::from_utf8_lossy(&ring).contains("LIVE-AFTER"),
        "live bytes lost from ring fallback after render"
    );
}

// ── PTY spawn + interaction (Unix; needs a real /dev/ptmx) ───────────────

#[cfg(unix)]
#[tokio::test]
async fn command_executes_directly_under_read_only_policy_object() {
    let sec = read_only_security();
    let session =
        PtyShellSession::spawn("printf direct-read-only", &sec, PtySize::default()).expect("direct PTY spawn");
    session.kill().await.ok();
}

#[cfg(unix)]
#[tokio::test]
async fn full_autonomy_executes_direct_command() {
    let sec = auto_security();
    let session = PtyShellSession::spawn("printf direct-full", &sec, PtySize::default()).expect("direct PTY spawn");
    session.kill().await.ok();
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
        let ring = session.attach_snapshot_for_test();
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
async fn kill_is_bounded_even_with_orphan_holding_slave() {
    // Blocker-2: `kill()` signals the group FIRST, then bounded-joins the drain
    // reader, so it must return within a bound even when a backgrounded
    // grandchild keeps the slave PTY fd open (so the master would never EOF on
    // its own). The poll-interruptible reader stops on the flag regardless, and
    // the bounded join caps any residual wait.
    let sec = auto_security();
    // The child backgrounds a long sleep that inherits the slave PTY, then the
    // foreground shell keeps running too. Killing the *group* reaches the
    // backgrounded sleep here (same pgid), but the bound must hold even if it
    // did not.
    let session = PtyShellSession::spawn("sleep 600 & sleep 600", &sec, PtySize::default())
        .expect("test: spawn child with backgrounded sleeper");

    // Let the child settle so its fds are open.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // `kill()` must complete well within a generous bound. The reap deadline is
    // 1s; with the SIGTERM grace (~300ms) plus join, 5s is a safe ceiling that
    // still fails loudly if `kill` ever blocks unboundedly.
    let start = std::time::Instant::now();
    let killed = tokio::time::timeout(std::time::Duration::from_secs(5), session.kill()).await;
    let elapsed = start.elapsed();
    assert!(
        killed.is_ok(),
        "kill() did not return within 5s — it blocked (orphan / unbounded join regression)"
    );
    killed.expect("test: kill within bound").expect("test: kill succeeds");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "kill() took too long ({elapsed:?})"
    );

    // Idempotent re-kill is also bounded.
    let re = tokio::time::timeout(std::time::Duration::from_secs(5), session.kill()).await;
    assert!(re.is_ok(), "idempotent kill() must also be bounded");
    re.expect("test: re-kill within bound")
        .expect("test: idempotent kill succeeds");
}

#[cfg(unix)]
#[tokio::test]
async fn pty_resize_is_accepted() {
    let sec = auto_security();
    let session = PtyShellSession::spawn("sleep 30", &sec, PtySize::default()).expect("test: spawn sleeper in a PTY");
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
    // Use the test snapshot path so we don't write the ring to the real
    // terminal during the test.
    let _ = session.attach_snapshot_for_test();
    session.detach();
    // Give any (wrong) teardown a chance to land.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!session.has_exited(), "detach must NOT kill the PTY child");
    assert!(session.is_attachable(), "a detached live PTY must remain attachable");

    // It is still functional: a re-attach + write still echoes.
    let _ = session.attach_snapshot_for_test();
    for &b in b"after-detach\n" {
        session.write_input(b).expect("test: write after re-attach");
    }
    let mut saw = false;
    for _ in 0..50 {
        if String::from_utf8_lossy(&session.attach_snapshot_for_test()).contains("after-detach") {
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
        "awk 'BEGIN { for (i = 1; i <= 200; i++) print \"line-\" i }'; sleep 5",
        &sec,
        PtySize::default(),
    )
    .expect("test: spawn chatty child");

    // Never attach. Poll the ring (snapshot peeks the buffer; we re-detach so
    // we never actually mirror to stdout in the test).
    let mut captured = String::new();
    for _ in 0..100 {
        let ring = session.attach_snapshot_for_test();
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
        captured.get(captured.len().saturating_sub(80)..)
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
        snapshot = session.attach_snapshot_for_test();
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
async fn emulator_screen_reflects_real_pty_output_for_reattach() {
    // v3b-b end-to-end: the persistent drain reader feeds the in-process
    // emulator, so a re-attach renders the CURRENT screen of a real PTY child
    // (not a raw byte replay). We run a real child that prints a marker, then
    // assert the emulator screen shows it — the bytes a re-attach would redraw.
    let sec = auto_security();
    let session = PtyShellSession::spawn("printf 'EMU-SCREEN-MARK'; sleep 5", &sec, PtySize::default())
        .expect("test: spawn printf child");

    let mut screen = String::new();
    for _ in 0..50 {
        screen = session.runtime.sink.screen_contents_for_test();
        if screen.contains("EMU-SCREEN-MARK") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        screen.contains("EMU-SCREEN-MARK"),
        "emulator screen must reflect real PTY output for re-attach redraw, got: {screen:?}"
    );

    // The render path produces a non-empty, screen-reconstructing redraw.
    let mut redraw: Vec<u8> = Vec::new();
    session.runtime.sink.attach_and_render_to(&mut redraw);
    session.detach();
    assert!(!redraw.is_empty(), "re-attach render of a live PTY must not be empty");

    session.kill().await.expect("test: cleanup kill");
}

#[cfg(unix)]
#[tokio::test]
async fn child_done_flag_set_when_child_exits() {
    // The drain reader observes EOF and sets child_done when the child exits on
    // its own, so the attach loop ends promptly.
    let sec = auto_security();
    let session = PtyShellSession::spawn("exit 0", &sec, PtySize::default()).expect("test: spawn fast-exiting child");
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
    let session = PtyShellSession::spawn("(setsid sleep 300 &) ; exit", &sec, PtySize::default())
        .expect("test: spawn shell that orphans a sleeper");

    // Give the shell time to spawn the orphan and exit; the orphan now holds
    // the slave open so no EOF will arrive at the master.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    session.kill_now(); // mirror the kill path; must NOT reap the orphan

    // `reap_reader` must return within a bounded window EVEN THOUGH EOF never
    // comes (orphan holds the slave). It runs on a blocking thread so the test
    // runtime is free.
    let session_for_reap = session.clone();
    let reaped = crate::runtime::blocking::spawn_blocking(move || {
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
