//! PTY-driven end-to-end tests for `prx chat`.
//!
//! These tests spawn the real `prx` binary inside a pseudo-terminal and assert
//! on the observable stdout/stderr stream. They cover regressions that pure
//! unit tests cannot catch (banner ordering, slash-command dispatch under TUI,
//! double-Ctrl-C semantics, ANSI escape elision under `--plain`).
//!
//! ## How it works
//!
//! - `test-mock` feature is required at build time (CI must pass
//!   `--features test-mock`). It adds the `mock` provider to the factory and
//!   to `list_providers()` so the availability check accepts it without an
//!   API key.
//! - Each test allocates a fresh `tempfile::TempDir` and points
//!   `OPENPRX_CONFIG_DIR` / `OPENPRX_WORKSPACE` / `HOME` / `XDG_DATA_HOME`
//!   into it, so the suite is fully hermetic — no `~/.openprx/` writes, no
//!   reedline history pollution.
//! - `PRX_TUI=0` forces the legacy reedline + `BufRead` fallback so the banner
//!   reliably lands on stdout (the ratatui path renders into the alt-screen
//!   and cannot be scraped by line-oriented matchers).
//! - reedline (and crossterm under the hood) probes cursor position via the
//!   DSR escape sequence `ESC [ 6 n` and blocks waiting for a `ESC [ <r> ;
//!   <c> R` reply that only a real terminal emulator generates. The
//!   `read_until_with_dsr` helper below scans the PTY stream and writes a
//!   synthetic `\x1b[1;1R` reply every time it sees a query, so the chat
//!   loop unblocks under expectrl.
//! - `serial_test::serial` keeps PTY tests from racing for controlling-tty
//!   resources without forcing `--test-threads=1` globally.
//!
//! ## Failure handling
//!
//! When the host has no usable PTY (rare on Linux but possible in some
//! containers), `expectrl::spawn` returns an `Err`. We treat that as a real
//! test failure (`panic!`) rather than silently passing — a silent skip
//! would mask "the test never actually ran" as "the test passed". Mark
//! these tests `#[ignore]` instead if you want to opt out per-environment.

#![cfg(feature = "test-mock")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::missing_const_for_fn,
    clippy::match_same_arms
)]

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use expectrl::session::Session;
use serial_test::serial;
use tempfile::TempDir;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const TURN_TIMEOUT: Duration = Duration::from_secs(6);
const EXIT_TIMEOUT: Duration = Duration::from_secs(4);

/// Synthetic reply to ESC[6n (Device Status Report → Report Cursor Position).
/// Real terminals respond with `ESC [ row ; col R`; we always answer 1;1.
const DSR_REPLY: &[u8] = b"\x1b[1;1R";
/// Query reedline / crossterm emits when probing the cursor.
const DSR_QUERY: &[u8] = b"\x1b[6n";

/// Holds the lifetime of the tempdir alongside the spawned session so any
/// late writes by the chat process land inside the sandbox (and clean up
/// when the test ends).
struct HarnessGuard {
    _tempdir: TempDir,
    config_dir: PathBuf,
    workspace_dir: PathBuf,
    home_dir: PathBuf,
    xdg_data: PathBuf,
    xdg_cache: PathBuf,
    xdg_state: PathBuf,
    xdg_config: PathBuf,
}

/// Locate the test-built `prx` binary. Cargo provides this via
/// `CARGO_BIN_EXE_<bin-name>` during `cargo test`.
fn prx_binary() -> &'static str {
    env!("CARGO_BIN_EXE_prx")
}

/// Build a fully-isolated `Command` for `prx chat`, redirecting every path the
/// chat session might write to into a fresh tempdir.
///
/// Caller is responsible for keeping the returned `HarnessGuard` alive for
/// the duration of the session (the `TempDir` is dropped together with it).
fn new_harness_guard() -> std::io::Result<HarnessGuard> {
    let tempdir = TempDir::new()?;
    let base = tempdir.path();
    let config_dir = base.join("config");
    let workspace_dir = base.join("workspace");
    let home_dir = base.join("home");
    let xdg_data = base.join("xdg_data");
    let xdg_cache = base.join("xdg_cache");
    let xdg_state = base.join("xdg_state");
    let xdg_config = base.join("xdg_config");
    for dir in [
        &config_dir,
        &workspace_dir,
        &home_dir,
        &xdg_data,
        &xdg_cache,
        &xdg_state,
        &xdg_config,
    ] {
        std::fs::create_dir_all(dir)?;
    }

    Ok(HarnessGuard {
        _tempdir: tempdir,
        config_dir,
        workspace_dir,
        home_dir,
        xdg_data,
        xdg_cache,
        xdg_state,
        xdg_config,
    })
}

fn build_chat_command_in(guard: &HarnessGuard, extra_args: &[&str], extra_env: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(prx_binary());
    cmd.arg("chat");
    cmd.arg("-p").arg("mock");
    cmd.arg("--model").arg("mock");
    for a in extra_args {
        cmd.arg(a);
    }
    // Wipe inherited env so the test doesn't accidentally pick up real
    // OpenPRX state from the developer's shell. Then layer the hermetic
    // env on top.
    cmd.env_clear();
    cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    cmd.env("OPENPRX_CONFIG_DIR", &guard.config_dir);
    cmd.env("OPENPRX_WORKSPACE", &guard.workspace_dir);
    cmd.env("HOME", &guard.home_dir);
    cmd.env("XDG_DATA_HOME", &guard.xdg_data);
    cmd.env("XDG_CACHE_HOME", &guard.xdg_cache);
    cmd.env("XDG_STATE_HOME", &guard.xdg_state);
    cmd.env("XDG_CONFIG_HOME", &guard.xdg_config);
    // Force reedline + BufRead fallback so banner lands on stdout.
    cmd.env("PRX_TUI", "0");
    cmd.env("NO_COLOR", "1");
    cmd.env("TERM", "xterm-256color");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");
    // Silence tracing so test output isn't polluted.
    cmd.env("RUST_LOG", "off");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    cmd
}

fn build_chat_command(extra_args: &[&str], extra_env: &[(&str, &str)]) -> std::io::Result<(Command, HarnessGuard)> {
    let guard = new_harness_guard()?;
    let cmd = build_chat_command_in(&guard, extra_args, extra_env);
    Ok((cmd, guard))
}

/// Spawn a `prx chat` PTY session, set a fixed 120x40 window, and configure
/// per-call expect timeouts. Panics when PTY is unavailable in the current
/// environment — a silent skip would mask "the regression test never
/// actually exercised the binary".
fn spawn_chat(extra_args: &[&str], extra_env: &[(&str, &str)]) -> (SessionGuard, HarnessGuard) {
    let (cmd, guard) = build_chat_command(extra_args, extra_env).expect("build chat command");
    let session = spawn_chat_command(cmd);
    (SessionGuard::new(session), guard)
}

fn spawn_chat_in(guard: &HarnessGuard, extra_args: &[&str], extra_env: &[(&str, &str)]) -> SessionGuard {
    let cmd = build_chat_command_in(guard, extra_args, extra_env);
    SessionGuard::new(spawn_chat_command(cmd))
}

fn spawn_chat_command(cmd: Command) -> Session {
    let mut session = match Session::spawn(cmd) {
        Ok(s) => s,
        Err(e) => {
            // Distinguish "no PTY device available" (ENXIO/ENOTTY, expected in some
            // containers) from real failures (binary not found, permission denied, etc.).
            let hint = if e.to_string().contains("No such device")
                || e.to_string().contains("not a tty")
                || e.to_string().contains("ENXIO")
                || e.to_string().contains("ENOTTY")
            {
                "This looks like a missing PTY device (ENXIO/ENOTTY). \
                 Mark the test `#[ignore = \"needs TTY\"]` if this host cannot \
                 allocate a pseudo-terminal."
            } else {
                "This does NOT look like a missing PTY device — check that the \
                 `prx` binary exists, is executable, and has the correct permissions."
            };
            panic!(
                "Failed to spawn PTY session: {e:?}\n\
                 Hint: {hint}"
            )
        }
    };
    // Fixed PTY geometry avoids width-dependent wrapping noise.
    if let Err(e) = session.get_process_mut().set_window_size(120, 40) {
        eprintln!("warning: set_window_size failed: {e}; continuing with default size");
    }
    session.set_expect_timeout(Some(TURN_TIMEOUT));
    session
}

/// RAII wrapper: guarantees `cleanup` runs even if the test panics during
/// expect/assert. Dropping the guard sends SIGKILL to any still-alive child
/// and drains the PTY stream so the OS releases the slave fd.
struct SessionGuard {
    inner: Option<Session>,
}

impl SessionGuard {
    fn new(session: Session) -> Self {
        Self { inner: Some(session) }
    }

    fn session(&mut self) -> &mut Session {
        // Invariant: `inner` is only `None` after explicit `into_session()` (none here)
        // or after `Drop` runs (no further access possible).
        self.inner.as_mut().expect("SessionGuard accessed after drop")
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(mut session) = self.inner.take() {
            if session.is_alive().unwrap_or(false) {
                let _ = session.get_process_mut().signal(expectrl::Signal::SIGKILL);
            }
            let mut sink = Vec::new();
            let _ = session.get_stream_mut().read_to_end(&mut sink);
        }
    }
}

/// Read from the PTY until `needle` appears in the accumulated buffer or
/// `deadline` is hit. Whenever a `ESC[6n` DSR cursor-position query is seen
/// in the stream, write back a synthetic `ESC[1;1R` reply so reedline /
/// crossterm don't block on a non-existent terminal emulator.
///
/// Returns the complete accumulated buffer on success. On timeout panics
/// with the captured buffer so the failure message is actionable.
fn read_until_with_dsr(session: &mut Session, needle: &str, total: Duration) -> String {
    let deadline = Instant::now() + total;
    let mut accum: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        // 1. drain any pending bytes
        match session.try_read(&mut buf) {
            Ok(0) => {
                // EOF
                let s = String::from_utf8_lossy(&accum).into_owned();
                if s.contains(needle) {
                    return s;
                }
                panic!("PTY hit EOF before `{needle}` appeared. Captured:\n----\n{s}\n----");
            }
            Ok(n) => {
                accum.extend_from_slice(&buf[..n]);
                // Reply to every DSR query we've seen but not yet acked.
                while let Some(pos) = window(&accum, DSR_QUERY) {
                    // Replace the query in-place with empty bytes so we don't
                    // re-detect on the next pass.
                    for b in &mut accum[pos..pos + DSR_QUERY.len()] {
                        *b = 0;
                    }
                    if let Err(e) = session.send(DSR_REPLY) {
                        eprintln!("warning: failed to send DSR reply: {e}");
                    }
                }
                let view = String::from_utf8_lossy(&accum);
                if view.contains(needle) {
                    return view.into_owned();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No data right now — yield.
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let s = String::from_utf8_lossy(&accum).into_owned();
                panic!("PTY read error before `{needle}`: {e}. Captured:\n----\n{s}\n----");
            }
        }
        if Instant::now() >= deadline {
            let s = String::from_utf8_lossy(&accum).into_owned();
            panic!(
                "timeout waiting for `{needle}` after {total:?}. Captured ({} bytes):\n----\n{s}\n----",
                accum.len()
            );
        }
    }
}

/// Reply once per `ESC[6n` occurrence seen in `chunk`, using a cross-chunk
/// accumulation buffer so queries that straddle read boundaries are never
/// missed. The caller passes `pending` by &mut so state persists across
/// multiple calls within the same polling loop.
///
/// A single read can return several DSR queries back-to-back (reedline prints
/// a few during init); replying only once leaves the others unanswered and
/// reedline parks forever.
fn ack_all_dsr_with_pending(session: &mut Session, chunk: &[u8], pending: &mut String) {
    // Append fresh bytes to the cross-chunk accumulation buffer.
    pending.push_str(&String::from_utf8_lossy(chunk));

    // Scan and consume all complete DSR queries from the front of the buffer.
    // We drain bytes as we go so the buffer never grows unboundedly.
    let dsr_query_str = "\x1b[6n";
    while let Some(idx) = pending.find(dsr_query_str) {
        // Discard everything before the query, then discard the query itself.
        pending.drain(..idx + dsr_query_str.len());
        if let Err(e) = session.send(DSR_REPLY) {
            eprintln!("warning: failed to send DSR reply: {e}");
            return;
        }
    }

    // After consuming all complete queries, keep only any trailing bytes that
    // might be the start of an incomplete ESC sequence (at most 3 bytes: an
    // `\x1b` that has not yet been followed by `[6n`). Everything else is
    // already-processed output that we don't need to hold on to.
    //
    // `\x1b[6n` is 4 bytes, so a tail shorter than 4 bytes starting with `\x1b`
    // is the only possible "half-sequence" we need to preserve.
    let drain_to = pending
        .rfind('\x1b')
        .filter(|&i| pending.len() - i < dsr_query_str.len())
        .unwrap_or(pending.len());
    // Ensure we drain only up to a valid UTF-8 char boundary.
    let safe_drain_to = (0..=drain_to).rev().find(|&i| pending.is_char_boundary(i)).unwrap_or(0);
    pending.drain(..safe_drain_to);
}

/// Drain any pending bytes from the PTY without blocking, replying to any
/// DSR queries encountered. Used between commands to keep reedline happy.
///
/// Uses a persistent `pending` buffer across chunks so that DSR queries
/// straddling read boundaries are never missed.
fn drain_with_dsr(session: &mut Session, total: Duration) {
    let deadline = Instant::now() + total;
    let mut buf = [0u8; 4096];
    let mut pending = String::new();
    while Instant::now() < deadline {
        match session.try_read(&mut buf) {
            Ok(0) => return,
            Ok(n) => ack_all_dsr_with_pending(session, &buf[..n], &mut pending),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return,
        }
    }
}

/// First-occurrence substring search byte-wise. Returned index is into the
/// haystack. None when not found.
fn window(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Wait up to `total` for the child process to exit. Returns true only when
/// the process has genuinely terminated (verified via `is_alive()` returning
/// `false`), not merely because the PTY fd produced a read error.
///
/// Previously, any non-WouldBlock read error was treated as "process exited",
/// which could hide hang regressions: a live process that triggers a transient
/// I/O error (e.g. EPIPE on the slave PTY side) would falsely look like it
/// exited. We now keep draining for DSR purposes but authoritative exit
/// detection is done exclusively via `is_alive()`.
fn wait_for_exit(session: &mut Session, total: Duration) -> bool {
    let deadline = Instant::now() + total;
    let mut pending_dsr = String::new();
    while Instant::now() < deadline {
        // Keep draining + replying to DSR while we wait so a still-alive
        // reedline doesn't sit blocked on a cursor query.
        let mut buf = [0u8; 4096];
        match session.try_read(&mut buf) {
            Ok(0) => {
                // EOF on the PTY stream — still need to confirm via is_alive().
            }
            Ok(n) => ack_all_dsr_with_pending(session, &buf[..n], &mut pending_dsr),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                // Log the error but do NOT treat it as proof of exit — the
                // process may still be alive with a broken PTY stream.
                eprintln!("wait_for_exit: PTY read error ({e}); continuing is_alive check");
            }
        }
        // Authoritative exit check: ask the OS whether the child is still alive.
        match session.is_alive() {
            Ok(false) => return true,
            Ok(true) => std::thread::sleep(Duration::from_millis(40)),
            Err(e) => {
                // is_alive() failure likely means the process has already been
                // reaped (e.g. ECHILD from waitpid) — treat as exited.
                eprintln!("wait_for_exit: is_alive() error ({e}); treating process as exited");
                return true;
            }
        }
    }
    false
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// 1. Banner is visible on startup.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_banner_visible() {
    let (mut sg, _guard) = spawn_chat(&[], &[]);
    let session = sg.session();

    // Banner format: "prx <ver> · mock/mock" (src/chat/mod.rs:634).
    let captured = read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    assert!(
        captured.contains("prx "),
        "banner should start with `prx `, got:\n{captured}"
    );
    assert!(
        captured.contains("mock/mock"),
        "banner should include `mock/mock`, got:\n{captured}"
    );

    // Clean exit. reedline raw mode treats CR as Enter.
    session.send("/exit\r").expect("send /exit");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
    // SessionGuard drop runs cleanup automatically (including on panic).
}

/// 2. A user message receives a mock response carrying our sentinel.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_mock_response_with_sentinel() {
    let sentinel = "[MOCK-END-A1B2]";
    let (mut sg, _guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", sentinel)]);
    let session = sg.session();

    // Wait for banner first so reedline is ready to consume our input.
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    // Send a user line. reedline expects CR for Enter under raw mode.
    session.send("hello\r").expect("send hello");

    // Mock provider returns our sentinel verbatim.
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(captured.contains(sentinel), "mock response should contain `{sentinel}`");

    // Clean exit.
    session.send("/exit\r").expect("send /exit");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

/// 3. `/exit` slash command terminates the session cleanly.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_exit_command_clean() {
    let (mut sg, _guard) = spawn_chat(&[], &[]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "process did not exit within {EXIT_TIMEOUT:?} after /exit"
    );

    // We accept any non-signal exit because the surface differs across
    // platforms; the regression we care about is the process not exiting
    // at all on /exit.
    if let Ok(status) = session.get_process().wait() {
        match status {
            expectrl::WaitStatus::Exited(_, code) => {
                assert_eq!(code, 0, "non-zero exit code after /exit: {code}");
            }
            other => panic!("unexpected wait status after /exit: {other:?}"),
        }
    }
}

/// 4. Double Ctrl-C exits the session — *both* the user-visible
/// `Exiting...` banner AND a fully-terminated process within the agreed
/// timeout. No fallback escalation (Ctrl-D / SIGHUP) is allowed; this
/// test is the regression detector for the runtime-drop hang bug.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_double_ctrl_c_exit() {
    let (mut sg, _guard) = spawn_chat(&[], &[]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    // First SIGINT — at idle this just stamps last_ctrlc_ms (see
    // src/chat/mod.rs:804). Within DOUBLE_CTRLC_WINDOW_MS a second SIGINT
    // must trigger graceful shutdown.
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");

    let captured = read_until_with_dsr(session, "Exiting...", EXIT_TIMEOUT);
    assert!(
        captured.contains("Exiting..."),
        "expected `Exiting...` after double Ctrl-C, captured:\n{captured}"
    );

    // ── Real exit contract: process must terminate ─────────────────────
    // The full double-Ctrl-C contract is: print `Exiting...` AND exit.
    // Previously the suite escalated via Ctrl-D / SIGHUP because reedline
    // parks in `spawn_blocking` on stdin; the runtime drop in `main` now
    // bounds that wait via `runtime.shutdown_timeout` (see main.rs's
    // `RUNTIME_SHUTDOWN_TIMEOUT`). If this assertion fails, the hang bug
    // has regressed.
    let exit_deadline = Duration::from_secs(6);
    assert!(
        wait_for_exit(session, exit_deadline),
        "process did NOT exit within {exit_deadline:?} after double Ctrl-C \
         even though `Exiting...` was emitted. This is the runtime-drop hang \
         bug — see main.rs RUNTIME_SHUTDOWN_TIMEOUT."
    );
}

/// 5. `--plain` mode means the mock response is emitted without ANSI color
/// escapes wrapping it. Cursor-positioning / DSR replies from reedline are
/// tolerated; what we assert is the absence of SGR colour codes in the
/// window between sending input and seeing the mock sentinel.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_plain_mode_no_ansi() {
    let sentinel = "[MOCK-PLAIN-C3D4]";
    let (mut sg, _guard) = spawn_chat(&["--plain"], &[("OPENPRX_MOCK_RESPONSE", sentinel)]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);

    // Inspect only the contiguous run of bytes immediately surrounding the
    // sentinel — i.e. the chat renderer's output for that line. reedline's
    // prompt elsewhere in the stream is allowed to emit SGR codes
    // (reedline's responsibility, not the chat plain-mode contract).
    //
    // The sentinel arrives on its own logical line: scan back to the
    // nearest newline or CSI cursor-positioning sequence, scan forward
    // similarly, and assert that slice has no SGR-coloured runs.
    let sentinel_pos = captured.rfind(sentinel).expect("sentinel was found above");
    // Scan backwards up to 256 bytes or to a newline / CR.
    let lookback_start = sentinel_pos.saturating_sub(256);
    let lookback = &captured[lookback_start..sentinel_pos];
    let start = lookback
        .rfind(['\n', '\r'])
        .map_or(lookback_start, |i| lookback_start + i + 1);
    // Scan forwards up to 256 bytes or to a newline / CR.
    let end_window = sentinel_pos + sentinel.len();
    let lookahead_end = (end_window + 256).min(captured.len());
    let lookahead = &captured[end_window..lookahead_end];
    let end = lookahead.find(['\n', '\r']).map_or(lookahead_end, |i| end_window + i);
    let line_window = &captured[start..end];

    let sgr_color = regex::Regex::new(r"\x1b\[[0-9;]*m").expect("sgr regex");
    let matches: Vec<&str> = sgr_color.find_iter(line_window).map(|m| m.as_str()).collect();
    assert!(
        matches.is_empty(),
        "SGR colour codes wrap the mock response under --plain: {matches:?}\nresponse line: {line_window:?}"
    );

    session.send("/exit\r").expect("send /exit");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

/// 6. Chinese (CJK) characters in the mock response must appear contiguously
/// — no phantom space between each character (regression guard for the
/// `insert_before` wide-char bug fixed by enabling the `scrolling-regions`
/// ratatui feature).
///
/// We use `--plain` mode (reedline + `BufRead`) so the response lands on
/// stdout verbatim, which lets the PTY scraper check the raw byte sequence.
#[test]
#[serial(prx_chat_pty)]
fn test_chat_chinese_response_no_extra_spaces() {
    // The mock response: 8 CJK chars with no ASCII filler in between.
    // If the wide-char bug is present, the captured output will contain
    // "你 好 世 界" (one space after each glyph).
    let response = "你好世界欢迎PRX";
    let (mut sg, _guard) = spawn_chat(&["--plain"], &[("OPENPRX_MOCK_RESPONSE", response)]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session.send("hi\r").expect("send hi");
    // Wait until the first CJK character appears in the output.
    let captured = read_until_with_dsr(session, "你", TURN_TIMEOUT);

    // Strip ANSI escape sequences from the captured buffer so we compare
    // plain text only. The DSR reply (`ESC[1;1R`) and reedline SGR codes
    // would otherwise pollute the search.
    let ansi_re = regex::Regex::new(r"\x1b\[[^a-zA-Z]*[a-zA-Z]").expect("ansi regex");
    let plain = ansi_re.replace_all(&captured, "").to_string();

    // The Chinese substring must appear as a contiguous run (no spaces).
    assert!(
        plain.contains("你好世界欢迎PRX"),
        "Chinese response should contain contiguous '你好世界欢迎PRX' (no inter-character spaces). \
         Plain output: {plain:?}"
    );
    // Explicit negative: spaces between consecutive CJK chars would indicate
    // the phantom-space bug has regressed.
    assert!(
        !plain.contains("你 好") && !plain.contains("好 世"),
        "phantom spaces detected between CJK chars in response. Plain output: {plain:?}"
    );

    session.send("/exit\r").expect("send /exit");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

// ─── Step 5a-1: Redux real-mode PTY 验证（PRX_CHAT_REDUX=both/1） ─────────────
//
// 这两个测试与上面 6 个的不同：通过 extra_env 把 PRX_CHAT_REDUX 显式注入子进程，
// 这样即使 env_clear 后子进程也能进入 Both/Redux 模式。它们覆盖：
//   - Both 模式下旧路径主导 + reducer 真业务执行不冲突（mock 回复仍正常）
//   - Redux 模式下双 Ctrl+C 仍能干净退出（round 2 hang bug 防回归核心）
//
// 注：这两个测试同样走 reedline 路径（PRX_TUI=0），并不能触达 ratatui
// `run_tui_unified_loop` 的 Ctrl+C 分支；ratatui 路径的退化覆盖在
// `src/chat/dispatcher.rs::real_mode_tests::ratatui_path_double_ctrlc_exits_via_reducer_and_executor`
// 中以单元测试形式存在。
#[test]
#[serial(prx_chat_pty)]
fn test_chat_redux_both_mock_response_works() {
    let sentinel = "[MOCK-REDUX-BOTH]";
    let (mut sg, _guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", sentinel), ("PRX_CHAT_REDUX", "both")]);
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "Both 模式下 mock 回复应仍能渲染。captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(wait_for_exit(session, EXIT_TIMEOUT), "Both 模式下 /exit 应干净退出");
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_redux_mode_double_ctrl_c_exit() {
    // Redux 模式 + 双 Ctrl+C：round 2 hang bug 防回归（PRX_CHAT_REDUX=1）.
    let (mut sg, _guard) = spawn_chat(&[], &[("PRX_CHAT_REDUX", "1")]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");

    let captured = read_until_with_dsr(session, "Exiting...", EXIT_TIMEOUT);
    assert!(
        captured.contains("Exiting..."),
        "Redux 模式下双 Ctrl+C 应输出 Exiting..., captured:\n{captured}"
    );

    let exit_deadline = Duration::from_secs(6);
    assert!(
        wait_for_exit(session, exit_deadline),
        "Redux 模式下双 Ctrl+C 应在 {exit_deadline:?} 内退出 — round 2 hang bug 未回归"
    );
}

// ─── Step 5a-4: Redux Driver 切闸 PTY 验证 ──────────────────────────────────
//
// PRX_CHAT_REDUX_DRIVER=1 + PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS=1 让路由
// 命中 ReduxDriver 路径：dispatch Action::StartLLMTurn → drive_start_turn_stream
// 通过 action_tx 回投 StreamCompleted → dispatcher task notify → chat::run await.
//
// 这两个测试与 5a-1 的不同：它们 **真正** 让 LLM turn 主路径走 dispatcher driver,
// 而非旧 run_tool_call_loop. 是 5a-4 "真切换"的关键验证.

#[test]
#[serial(prx_chat_pty)]
fn test_chat_redux_driver_mock_response_works() {
    // PRX_CHAT_REDUX=1 + DRIVER=1 + FORCE_EMPTY_TOOLS=1 → 走 dispatcher driver.
    // mock provider 默认 sentinel 返回最终文本，drive_start_turn_stream 应把它
    // 通过 StreamChunkReceived → StreamCompleted 投递到 reducer，再由 dispatcher
    // task 触发 turn_signal，chat::run 拿到 final_text 渲染给用户.
    let sentinel = "[MOCK-REDUX-DRIVER]";
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_RESPONSE", sentinel),
            ("PRX_CHAT_REDUX", "1"),
            ("PRX_CHAT_REDUX_DRIVER", "1"),
            ("PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS", "1"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "Redux driver 路径下 mock 回复应仍能渲染。captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "Redux driver 路径下 /exit 应干净退出"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_redux_driver_tool_call_executes_and_completes() {
    // 5a-6 happy path E2E: PRX_CHAT_REDUX=1 + DRIVER=1 (无 FORCE_EMPTY_TOOLS), 让 driver
    // 真接 tools_registry; mock provider 通过 OPENPRX_MOCK_TOOL_CALL 第一轮 emit
    // tool_call("memory_recall", {"query":"hi"}), driver 执行后第二轮 emit final 文本.
    //
    // 关键断言: 用户输入 → driver 调 tool → tool 完成 → 第二轮 LLM 调用拿 final
    // sentinel → 渲染给用户。FORCE_EMPTY_TOOLS 没有设, 保证 tools_registry 真有内容.
    let sentinel = "[MOCK-TOOL-ROUND-2-OK]";
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_RESPONSE", sentinel),
            ("OPENPRX_MOCK_TOOL_CALL", "memory_recall:{\"query\":\"hi\",\"limit\":3}"),
            ("PRX_CHAT_REDUX", "1"),
            ("PRX_CHAT_REDUX_DRIVER", "1"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "driver tool-call 闭环: 第二轮 LLM 应返回 final sentinel. captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "driver 工具回合后 /exit 应干净退出"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_redux_driver_double_ctrl_c_no_round2_hang() {
    // 5a-4 关键防回归：driver 路径真接 dispatcher 后双 Ctrl+C 不能 hang round 2.
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("PRX_CHAT_REDUX", "1"),
            ("PRX_CHAT_REDUX_DRIVER", "1"),
            ("PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS", "1"),
        ],
    );
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");

    let captured = read_until_with_dsr(session, "Exiting...", EXIT_TIMEOUT);
    assert!(
        captured.contains("Exiting..."),
        "driver 路径下双 Ctrl+C 应输出 Exiting...; captured:\n{captured}"
    );

    let exit_deadline = Duration::from_secs(6);
    assert!(
        wait_for_exit(session, exit_deadline),
        "driver 路径下双 Ctrl+C 应在 {exit_deadline:?} 内退出 — round 2 hang bug 未回归"
    );
}

// ─── T3-3-d: Pure 模式 PTY E2E 覆盖 ───────────────────────────────────────────
//
// `PRX_CHAT_REDUX=pure`：T3-3 收官模式 — reducer 单路由，driver 默认开（无需
// `PRX_CHAT_REDUX_DRIVER=1`），legacy `chat_session.add_*_turn` 不再执行，
// reducer 的 `Effect::SaveSession` 接管持久化。
//
// 这两个测试是 T3-3-d 的关键防回归：
//   - mock_response_works: 验证 Pure 模式端到端对话能跑通（输入→驱动→渲染）
//   - tool_call_completes: 验证 Pure 模式 driver 自动 attach tools_registry
//                          （不依赖 FORCE_EMPTY_TOOLS backdoor）

#[test]
#[serial(prx_chat_pty)]
fn test_chat_redux_pure_mock_response_works() {
    // Pure 模式 + mock provider：driver 路径默认开（无 PRX_CHAT_REDUX_DRIVER），
    // 用户输入 → driver streams → reducer StreamCompleted → SaveSession Effect →
    // memory.store（本测试只断言 sentinel 渲染到 PTY，持久化由单测覆盖）.
    let sentinel = "[MOCK-REDUX-PURE]";
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_RESPONSE", sentinel),
            ("PRX_CHAT_REDUX", "pure"),
            // 故意不设 PRX_CHAT_REDUX_DRIVER；Pure 模式必须默认走 driver.
            ("PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS", "1"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "Pure 模式下 mock 回复应渲染. captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(wait_for_exit(session, EXIT_TIMEOUT), "Pure 模式下 /exit 应干净退出");
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_redux_pure_tool_call_completes() {
    // Pure 模式 + 真 tools_registry（无 FORCE_EMPTY_TOOLS）：driver 在 Pure 下
    // 必须默认 attach tools_registry 才能完成 tool turn 闭环。
    let sentinel = "[MOCK-PURE-TOOL-ROUND-2]";
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_RESPONSE", sentinel),
            ("OPENPRX_MOCK_TOOL_CALL", "memory_recall:{\"query\":\"hi\",\"limit\":3}"),
            ("PRX_CHAT_REDUX", "pure"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "Pure 模式 driver tool-call 闭环应返回 final sentinel. captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "Pure 模式工具回合后 /exit 应干净退出"
    );
}

/// fixA P1-5: chat::run 级集成测试 — Pure 模式跑完整 turn + /exit，验证 exit save
/// 守卫整条路径（top_redux_mode → legacy_exit_save_enabled=false → reducer 单源持久化）.
/// 单独的 reducer 单测无法触达 chat::run 主循环的退出分支；本测试通过真 PTY 走完整路径.
#[test]
#[serial(prx_chat_pty)]
fn s4_a_p1_pure_exit_after_turn_chat_run_level() {
    let sentinel = "[MOCK-PURE-EXIT]";
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_RESPONSE", sentinel),
            ("PRX_CHAT_REDUX", "pure"),
            ("PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS", "1"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    // turn 1: 完整 user → assistant final
    session.send("hi\r").expect("send hi");
    let captured1 = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured1.contains(sentinel),
        "Pure chat::run 应渲染 turn 1 sentinel. captured:\n{captured1}"
    );

    // /exit 必须穿过 chat::run 主循环 break + legacy_exit_save_enabled=false 路径
    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "Pure chat::run 完整 turn 后 /exit 应在 {EXIT_TIMEOUT:?} 内干净退出（reducer-only save 路径）"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_session_resume_last_restores_saved_turns() {
    let first_response = "[MOCK-RESUME-FIRST]";
    let second_response = "[MOCK-RESUME-SECOND]";
    let first_message = format!("resume-check-{}", std::process::id());

    let (mut sg, guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", first_response)]);
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session
        .send(format!("{first_message}\r").as_str())
        .expect("send first message");
    let captured = read_until_with_dsr(session, first_response, TURN_TIMEOUT);
    assert!(
        captured.contains(first_response),
        "first chat process should complete turn before resume test. captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(wait_for_exit(session, EXIT_TIMEOUT), "first chat process should exit");
    drop(sg);

    let before_resume = build_chat_command_in(&guard, &["--list-sessions"], &[])
        .output()
        .expect("list sessions before resume");
    assert!(
        before_resume.status.success(),
        "list sessions before resume failed: status={:?}, stderr={}",
        before_resume.status,
        String::from_utf8_lossy(&before_resume.stderr)
    );
    let before_stdout = String::from_utf8_lossy(&before_resume.stdout);
    assert!(
        before_stdout.contains(&first_message) && before_stdout.contains("2 turns"),
        "first session should be saved with one user/assistant pair. stdout:\n{before_stdout}"
    );

    let mut sg2 = spawn_chat_in(
        &guard,
        &["--session", "last"],
        &[("OPENPRX_MOCK_RESPONSE", second_response)],
    );
    let session2 = sg2.session();
    read_until_with_dsr(session2, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session2, Duration::from_millis(200));
    session2.send("resume followup\r").expect("send second message");
    let captured2 = read_until_with_dsr(session2, second_response, TURN_TIMEOUT);
    assert!(
        captured2.contains(second_response),
        "resumed chat process should complete follow-up turn. captured:\n{captured2}"
    );
    session2.send("/exit\r").expect("send /exit resumed");
    assert!(
        wait_for_exit(session2, EXIT_TIMEOUT),
        "resumed chat process should exit"
    );
    drop(sg2);

    let after_resume = build_chat_command_in(&guard, &["--list-sessions"], &[])
        .output()
        .expect("list sessions after resume");
    assert!(
        after_resume.status.success(),
        "list sessions after resume failed: status={:?}, stderr={}",
        after_resume.status,
        String::from_utf8_lossy(&after_resume.stderr)
    );
    let after_stdout = String::from_utf8_lossy(&after_resume.stdout);
    assert!(
        after_stdout.contains(&first_message) && after_stdout.contains("4 turns"),
        "`--session last` should append the follow-up to the original saved session. stdout:\n{after_stdout}"
    );
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_redux_pure_double_ctrl_c_exits_cleanly() {
    // Pure 模式下双 Ctrl+C 不能 hang round 2（与 Redux 模式同样的 round-2 hang 防回归）.
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("PRX_CHAT_REDUX", "pure"),
            ("PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS", "1"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");

    let captured = read_until_with_dsr(session, "Exiting...", EXIT_TIMEOUT);
    assert!(
        captured.contains("Exiting..."),
        "Pure 模式下双 Ctrl+C 应输出 Exiting...; captured:\n{captured}"
    );
    let exit_deadline = Duration::from_secs(6);
    assert!(
        wait_for_exit(session, exit_deadline),
        "Pure 模式下双 Ctrl+C 应在 {exit_deadline:?} 内退出"
    );
}

// ─── S4-A Commit 0: ratatui 真路径最小 E2E ────────────────────────────────────
//
// 现有 14 个 PTY 测试都把 `PRX_TUI=0` 注入子进程，落在 reedline + BufRead
// fallback；ratatui 真路径（`run_tui_unified_loop`）零回归保护。S4-A 切换
// 渲染源前必须先有真路径回归保护，故新增 3 个测试覆盖：
//   - banner 渲染（启动可见）
//   - mock response 流式渲染
//   - double Ctrl+C 退出
//
// 通过 `extra_env` 注入 `PRX_TUI=1` 覆盖默认 `PRX_TUI=0`，让 chat::run
// 走 `TerminalGuard::enter()` + `spawn_tui_unified_loop`。ratatui 用
// `Viewport::Inline` 不进 alt-screen，bytes 仍走主缓冲可被 PTY scraper
// 抓到。banner 通过 `chat_mirror.lock().push_system_message(&banner)` +
// 后续 `terminal.insert_before` 写到 stdout（mod.rs:1084 + 2590）.

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_0_ratatui_banner_visible_via_real_path() {
    // PRX_TUI=1 强制走 ratatui 真路径。banner 通过 insert_before 写入主屏
    // scrollback，PTY scraper 能拿到 "mock/mock" 字串。
    let (mut sg, _guard) = spawn_chat(&[], &[("PRX_TUI", "1")]);
    let session = sg.session();

    let captured = read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    assert!(captured.contains("prx "), "banner 应以 `prx ` 起头, got:\n{captured}");
    assert!(
        captured.contains("mock/mock"),
        "banner 应包含 `mock/mock`, got:\n{captured}"
    );

    // 双 Ctrl+C 退出（/exit 在 ratatui 路径下也工作，但发送 \r 后 ratatui
    // raw mode 的 line discipline 与 reedline 不同，用 SIGINT*2 更稳）.
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_0_ratatui_mock_response_via_real_path() {
    // ratatui 真路径下，用户输入 → mock provider 流式回 sentinel → ratatui
    // 把 ConversationLine::Assistant insert_before 到主屏。PTY scraper
    // 应能在主屏看到 sentinel。
    let sentinel = "[S4A0-RATATUI-MOCK]";
    let (mut sg, _guard) = spawn_chat(&[], &[("OPENPRX_MOCK_RESPONSE", sentinel), ("PRX_TUI", "1")]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(300));

    // ratatui raw mode 下 Enter 仍是 \r（crossterm KeyCode::Enter）.
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "ratatui 真路径下 mock 回复应渲染. captured:\n{captured}"
    );

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_0_ratatui_double_ctrl_c_exit_via_real_path() {
    // ratatui 真路径下双 Ctrl+C 退出语义 — `run_tui_unified_loop` 内
    // `KeyDispatch::InterruptTurn` 分支 + shutdown.cancel() 路径.
    let (mut sg, _guard) = spawn_chat(&[], &[("PRX_TUI", "1")]);
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");

    let exit_deadline = Duration::from_secs(6);
    assert!(
        wait_for_exit(session, exit_deadline),
        "ratatui 真路径下双 Ctrl+C 应在 {exit_deadline:?} 内退出"
    );
}

// ─── S4-A Commit 6: ratatui 真路径 + Pure 模式 snapshot 端到端 ──────────────
//
// 把 Commit 0 三个最小 E2E 升级为 PRX_TUI=1 + PRX_CHAT_REDUX=pure 组合,
// 验证 S4-A 完成后:
//   - reducer 单源驱动 ratatui 渲染 (UiSnapshot watch 路径)
//   - chat_mirror 在 Pure 下零写入 (单一写源原则)
//   - banner / mock response / tool call / 中文 都能正确渲染

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_6_pure_snapshot_renders_banner_via_real_path() {
    // ratatui 真路径 + Pure 模式. banner 通过 reducer SystemMessageAdded
    // 写入 ui.conversation_lines, dispatcher 推 snapshot 到 watch,
    // run_tui_unified_loop 通过 RenderSource::Snapshot 读取并 insert_before
    // 到主屏 — PTY scraper 应能拿到 "mock/mock" 字符串.
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("PRX_TUI", "1"),
            ("PRX_CHAT_REDUX", "pure"),
            ("PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS", "1"),
        ],
    );
    let session = sg.session();

    let captured = read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    assert!(captured.contains("prx "), "banner 应以 `prx ` 起头, got:\n{captured}");
    assert!(
        captured.contains("mock/mock"),
        "Pure 模式 ratatui 真路径下 banner 应含 `mock/mock`, got:\n{captured}"
    );

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_6_pure_snapshot_mock_response_via_real_path() {
    // Pure 模式 + ratatui 真路径下流式 mock 回复:
    // 用户输入 → drive_start_turn_stream dispatch StreamChunkReceived /
    // StreamCompleted → reducer push ConversationLine::Assistant →
    // dispatcher 推 UiSnapshot → run_tui_unified_loop insert_before 主屏.
    let sentinel = "[S4A6-PURE-RATATUI]";
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_RESPONSE", sentinel),
            ("PRX_TUI", "1"),
            ("PRX_CHAT_REDUX", "pure"),
            ("PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS", "1"),
        ],
    );
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(300));

    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "Pure ratatui 真路径下 mock 回复应渲染. captured:\n{captured}"
    );

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_6_pure_snapshot_tool_call_via_real_path() {
    // Pure 模式 + ratatui 真路径下 tool turn 闭环:
    // 第一轮 LLM dispatch ToolStarted → reducer push ToolResult Running →
    // driver 执行 tool → ToolFinished → reducer 更新 ToolResult Done →
    // 第二轮 LLM emit final sentinel.
    let sentinel = "[S4A6-PURE-TOOL]";
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_RESPONSE", sentinel),
            ("OPENPRX_MOCK_TOOL_CALL", "memory_recall:{\"query\":\"hi\",\"limit\":3}"),
            ("PRX_TUI", "1"),
            ("PRX_CHAT_REDUX", "pure"),
        ],
    );
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(300));

    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "Pure ratatui 真路径 tool-call 闭环应返回 final sentinel. captured:\n{captured}"
    );

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

#[test]
#[serial(prx_chat_pty)]
fn test_chat_s4_a_6_pure_snapshot_chinese_no_extra_spaces_via_real_path() {
    // Pure + ratatui 真路径下中文（CJK）响应应字节级正确, 无 phantom space.
    let response = "你好世界S4A6";
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_RESPONSE", response),
            ("PRX_TUI", "1"),
            ("PRX_CHAT_REDUX", "pure"),
            ("PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS", "1"),
        ],
    );
    let session = sg.session();

    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(300));

    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, "你", TURN_TIMEOUT);
    // 剥离 ANSI escape 后断言中文连续.
    let ansi_re = regex::Regex::new(r"\x1b\[[^a-zA-Z]*[a-zA-Z]").expect("ansi regex");
    let plain = ansi_re.replace_all(&captured, "").to_string();
    assert!(
        plain.contains("你好世界S4A6"),
        "Pure ratatui 真路径下中文应连续, plain:\n{plain}"
    );
    assert!(
        !plain.contains("你 好") && !plain.contains("好 世"),
        "phantom space 应不存在, plain:\n{plain}"
    );

    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    let _ = wait_for_exit(session, EXIT_TIMEOUT);
}

// ─── S5 P0-1: 协议级 PTY 回归 (anthropic / openai / gemini flavor) ─────────────
//
// 无 API key 时真实 LLM 不可达，本组测试通过 MockEnvProvider OPENPRX_MOCK_SCRIPT
// 在 streaming 路径 emit 完整脚本（delta + reasoning + tool + final），覆盖
// driver 在不同 provider flavor 下的协议层回归。详见 docs/release-notes-0.4.0.md。

/// S5 P0-1: anthropic flavor 完整 turn — 多个 delta + reasoning + final.
#[test]
#[serial(prx_chat_pty)]
fn s5_release_p0_1_anthropic_full_turn_via_real_path() {
    let sentinel = "[MOCK-S5-P0-1-ANTHROPIC]";
    let script = format!(
        r#"{{"chunks":[
            {{"delta":"Hello "}},
            {{"delta":"from "}},
            {{"delta":"anthropic "}},
            {{"delta":"flavor "}},
            {{"delta":"{sentinel}"}},
            {{"is_final":true}}
        ]}}"#
    );
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_SCRIPT", script.as_str()),
            ("OPENPRX_MOCK_PROVIDER_FLAVOR", "anthropic"),
            ("PRX_CHAT_REDUX", "pure"),
            ("PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS", "1"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "anthropic flavor 应汇出 sentinel. captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "anthropic flavor /exit 应干净退出"
    );
}

/// S5 P0-1: openai flavor 含 tool_call 的 turn — driver 必须执行 tool 再续 text.
#[test]
#[serial(prx_chat_pty)]
fn s5_release_p0_1_openai_tool_call_turn_via_real_path() {
    let sentinel = "[MOCK-S5-P0-1-OPENAI-DONE]";
    let script = format!(
        r#"{{"chunks":[
            {{"tool":{{"id":"t1","name":"memory_recall","args":"{{\"query\":\"x\",\"limit\":1}}"}}}},
            {{"delta":"{sentinel}"}},
            {{"is_final":true}}
        ]}}"#
    );
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_SCRIPT", script.as_str()),
            ("OPENPRX_MOCK_PROVIDER_FLAVOR", "openai"),
            ("PRX_CHAT_REDUX", "pure"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    let captured = read_until_with_dsr(session, sentinel, TURN_TIMEOUT);
    assert!(
        captured.contains(sentinel),
        "openai flavor tool turn 应最终汇出 sentinel. captured:\n{captured}"
    );
    session.send("/exit\r").expect("send /exit");
    assert!(wait_for_exit(session, EXIT_TIMEOUT), "openai flavor /exit 应干净退出");
}

/// S5 P0-1: gemini flavor cancel-mid-stream — 第一个 chunk 出现后双 Ctrl+C.
#[test]
#[serial(prx_chat_pty)]
fn s5_release_p0_1_gemini_cancel_midstream_via_real_path() {
    // 10 chunks * 100ms = 1s 总时长，足够双 Ctrl+C 落在中间.
    let script = r#"{"chunks":[
        {"delta":"g1 "},
        {"delta":"g2 "},
        {"delta":"g3 "},
        {"delta":"g4 "},
        {"delta":"g5 "},
        {"delta":"g6 "},
        {"delta":"g7 "},
        {"delta":"g8 "},
        {"delta":"g9 "},
        {"delta":"g10"},
        {"is_final":true}
    ]}"#;
    let (mut sg, _guard) = spawn_chat(
        &[],
        &[
            ("OPENPRX_MOCK_SCRIPT", script),
            ("OPENPRX_MOCK_PROVIDER_FLAVOR", "gemini"),
            ("OPENPRX_MOCK_DELAY_MS_PER_CHUNK", "100"),
            ("PRX_CHAT_REDUX", "pure"),
            ("PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS", "1"),
        ],
    );
    let session = sg.session();
    read_until_with_dsr(session, "mock/mock", STARTUP_TIMEOUT);
    drain_with_dsr(session, Duration::from_millis(200));
    session.send("hi\r").expect("send hi");
    read_until_with_dsr(session, "g1 ", TURN_TIMEOUT);
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("first SIGINT");
    std::thread::sleep(Duration::from_millis(100));
    session
        .get_process_mut()
        .signal(expectrl::Signal::SIGINT)
        .expect("second SIGINT");
    assert!(
        wait_for_exit(session, EXIT_TIMEOUT),
        "gemini flavor cancel-mid-stream 应在 {EXIT_TIMEOUT:?} 内退出"
    );
}
