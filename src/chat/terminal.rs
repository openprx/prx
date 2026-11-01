//! Terminal mode guard, panic hook and chat tracing redirection.

use anyhow::Result;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[cfg(feature = "terminal-tui")]
pub(super) fn mouse_capture_disabled_by_env(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

#[cfg(feature = "terminal-tui")]
pub(super) fn mouse_capture_enabled_by_env(disable_value: Option<&str>) -> bool {
    if mouse_capture_disabled_by_env(disable_value) {
        return false;
    }
    true
}

// ── P3-2 / alt-screen Phase 0: TerminalGuard RAII + panic restore ────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "terminal-tui")]
pub(super) struct ChatTuiSelection {
    pub(super) enabled: bool,
}

#[cfg(feature = "terminal-tui")]
pub(super) fn select_chat_tui(
    plain_mode: bool,
    stdin_is_terminal: bool,
    prx_tui_env: Option<&str>,
) -> ChatTuiSelection {
    let enabled = should_enable_terminal_tui(plain_mode, stdin_is_terminal, prx_tui_env);
    ChatTuiSelection { enabled }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TerminalGuardState {
    pub(super) raw_mode_active: bool,
    pub(super) bracketed_paste_active: bool,
    pub(super) keyboard_enhancement_active: bool,
    pub(super) mouse_capture_active: bool,
    pub(super) alternate_screen_active: bool,
}

impl TerminalGuardState {
    #[cfg(feature = "terminal-tui")]
    pub(super) const fn inactive() -> Self {
        Self {
            raw_mode_active: false,
            bracketed_paste_active: false,
            keyboard_enhancement_active: false,
            mouse_capture_active: false,
            alternate_screen_active: false,
        }
    }
}

pub(super) trait TerminalModeOps {
    #[cfg(feature = "terminal-tui")]
    fn enable_raw_mode(&mut self) -> std::io::Result<()>;
    fn disable_raw_mode(&mut self) -> std::io::Result<()>;
    #[cfg(feature = "terminal-tui")]
    fn supports_keyboard_enhancement(&mut self) -> std::io::Result<bool>;
    #[cfg(feature = "terminal-tui")]
    fn push_keyboard_enhancement_flags(&mut self) -> std::io::Result<()>;
    fn pop_keyboard_enhancement_flags(&mut self) -> std::io::Result<()>;
    #[cfg(feature = "terminal-tui")]
    fn enable_bracketed_paste(&mut self) -> std::io::Result<()>;
    fn disable_bracketed_paste(&mut self) -> std::io::Result<()>;
    #[cfg(feature = "terminal-tui")]
    fn enable_mouse_capture(&mut self) -> std::io::Result<()>;
    fn disable_mouse_capture(&mut self) -> std::io::Result<()>;
    #[cfg(feature = "terminal-tui")]
    fn enter_alternate_screen(&mut self) -> std::io::Result<()>;
    fn leave_alternate_screen(&mut self) -> std::io::Result<()>;
    fn show_cursor(&mut self) -> std::io::Result<()>;
}

struct CrosstermTerminalModeOps;

#[cfg(feature = "terminal-tui")]
pub(super) fn chat_keyboard_enhancement_flags() -> crossterm::event::KeyboardEnhancementFlags {
    // Crossterm 0.29 does not expose kitty's REPORT_ASSOCIATED_TEXT flag.
    // REPORT_ALL_KEYS_AS_ESCAPE_CODES without associated text causes IME
    // commits (for example, Chinese input in iTerm2) to lose their text.
    // Keep ordinary text as UTF-8 while retaining the enhancements needed for
    // unambiguous shortcuts and key press/release handling.
    crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | crossterm::event::KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        | crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES
}

impl TerminalModeOps for CrosstermTerminalModeOps {
    #[cfg(feature = "terminal-tui")]
    fn enable_raw_mode(&mut self) -> std::io::Result<()> {
        crossterm::terminal::enable_raw_mode()
    }

    fn disable_raw_mode(&mut self) -> std::io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }

    #[cfg(feature = "terminal-tui")]
    fn supports_keyboard_enhancement(&mut self) -> std::io::Result<bool> {
        crossterm::terminal::supports_keyboard_enhancement()
    }

    #[cfg(feature = "terminal-tui")]
    fn push_keyboard_enhancement_flags(&mut self) -> std::io::Result<()> {
        let flags = chat_keyboard_enhancement_flags();
        crossterm::execute!(std::io::stdout(), crossterm::event::PushKeyboardEnhancementFlags(flags)).map(|_| ())
    }

    fn pop_keyboard_enhancement_flags(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::PopKeyboardEnhancementFlags).map(|_| ())
    }

    #[cfg(feature = "terminal-tui")]
    fn enable_bracketed_paste(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste).map(|_| ())
    }

    fn disable_bracketed_paste(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste).map(|_| ())
    }

    #[cfg(feature = "terminal-tui")]
    fn enable_mouse_capture(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture).map(|_| ())
    }

    fn disable_mouse_capture(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture).map(|_| ())
    }

    #[cfg(feature = "terminal-tui")]
    fn enter_alternate_screen(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen).map(|_| ())
    }

    fn leave_alternate_screen(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen).map(|_| ())
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        crossterm::execute!(std::io::stdout(), crossterm::cursor::Show).map(|_| ())
    }
}

pub(super) static CHAT_FULLSCREEN_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) static CHAT_KEYBOARD_ENHANCEMENT_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(super) static CHAT_MOUSE_CAPTURE_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "terminal-tui")]
pub(super) fn enter_terminal_state_with_ops(ops: &mut impl TerminalModeOps) -> std::io::Result<TerminalGuardState> {
    let mouse_enabled = mouse_capture_enabled_by_env(std::env::var("PRX_TUI_DISABLE_MOUSE").ok().as_deref());
    enter_terminal_state_with_ops_inner(ops, mouse_enabled)
}

#[cfg(feature = "terminal-tui")]
pub(super) fn enter_terminal_state_with_ops_inner(
    ops: &mut impl TerminalModeOps,
    mouse_enabled: bool,
) -> std::io::Result<TerminalGuardState> {
    let mut state = TerminalGuardState::inactive();

    ops.enable_raw_mode()?;
    state.raw_mode_active = true;

    CHAT_FULLSCREEN_ACTIVE.store(true, std::sync::atomic::Ordering::Release);
    if let Err(e) = ops.enter_alternate_screen() {
        CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        let _ = ops.disable_raw_mode();
        return Err(e);
    }
    state.alternate_screen_active = true;

    if mouse_enabled {
        if let Err(e) = ops.enable_mouse_capture() {
            if state.alternate_screen_active {
                let _ = ops.leave_alternate_screen();
                CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
            }
            let _ = ops.disable_raw_mode();
            return Err(e);
        }
        state.mouse_capture_active = true;
        CHAT_MOUSE_CAPTURE_ACTIVE.store(true, std::sync::atomic::Ordering::Release);
    }

    match ops.supports_keyboard_enhancement() {
        Ok(true) => {
            if let Err(e) = ops.push_keyboard_enhancement_flags() {
                if state.mouse_capture_active {
                    let _ = ops.disable_mouse_capture();
                    CHAT_MOUSE_CAPTURE_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
                }
                if state.alternate_screen_active {
                    let _ = ops.leave_alternate_screen();
                    CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
                }
                let _ = ops.disable_raw_mode();
                return Err(e);
            }
            state.keyboard_enhancement_active = true;
            CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.store(true, std::sync::atomic::Ordering::Release);
        }
        Ok(false) => {}
        Err(e) => {
            tracing::debug!(error = %e, "terminal keyboard enhancement probe failed; skipping");
        }
    }

    if let Err(e) = ops.enable_bracketed_paste() {
        if state.keyboard_enhancement_active {
            let _ = ops.pop_keyboard_enhancement_flags();
            CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        }
        if state.mouse_capture_active {
            let _ = ops.disable_mouse_capture();
            CHAT_MOUSE_CAPTURE_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        }
        if state.alternate_screen_active {
            let _ = ops.leave_alternate_screen();
            CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        }
        let _ = ops.disable_raw_mode();
        return Err(e);
    }
    state.bracketed_paste_active = true;

    Ok(state)
}

pub(super) fn leave_terminal_state_with_ops(ops: &mut impl TerminalModeOps, state: TerminalGuardState) {
    if state.bracketed_paste_active {
        let _ = ops.disable_bracketed_paste();
        let _ = ops.show_cursor();
    }
    if state.keyboard_enhancement_active {
        let _ = ops.pop_keyboard_enhancement_flags();
        CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
    }
    if state.mouse_capture_active {
        let _ = ops.disable_mouse_capture();
        CHAT_MOUSE_CAPTURE_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
    }
    if state.alternate_screen_active {
        let _ = ops.leave_alternate_screen();
        CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
    }
    if state.raw_mode_active {
        let _ = ops.disable_raw_mode();
    }
}

pub(super) fn restore_terminal_state_with_ops(ops: &mut impl TerminalModeOps, leave_alternate_screen: bool) {
    let state = TerminalGuardState {
        raw_mode_active: true,
        bracketed_paste_active: true,
        keyboard_enhancement_active: CHAT_KEYBOARD_ENHANCEMENT_ACTIVE.swap(false, std::sync::atomic::Ordering::AcqRel),
        mouse_capture_active: CHAT_MOUSE_CAPTURE_ACTIVE.swap(false, std::sync::atomic::Ordering::AcqRel),
        alternate_screen_active: leave_alternate_screen,
    };
    leave_terminal_state_with_ops(ops, state);
}

/// Best-effort terminal restoration used by both [`TerminalGuard`] and the chat
/// panic hook installed via [`install_chat_panic_hook`].
///
/// Fullscreen mode records a process-global active flag so panic restore can
/// emit `LeaveAlternateScreen` before the chained hook prints its backtrace.
pub(super) fn restore_terminal_state() {
    let leave_alternate_screen = CHAT_FULLSCREEN_ACTIVE.swap(false, std::sync::atomic::Ordering::AcqRel);
    let mut ops = CrosstermTerminalModeOps;
    restore_terminal_state_with_ops(&mut ops, leave_alternate_screen);
}

/// RAII guard for the chat TUI terminal state.
///
/// The fullscreen TUI lifecycle is raw mode + alternate screen + bracketed
/// paste. `enter()` is transactional: any partial failure rolls back
/// already-applied terminal state before returning `Err`.
pub struct TerminalGuard {
    pub(super) raw_mode_active: std::sync::atomic::AtomicBool,
    pub(super) bracketed_paste_active: std::sync::atomic::AtomicBool,
    pub(super) keyboard_enhancement_active: std::sync::atomic::AtomicBool,
    pub(super) mouse_capture_active: std::sync::atomic::AtomicBool,
    pub(super) alternate_screen_active: std::sync::atomic::AtomicBool,
}

impl TerminalGuard {
    #[cfg(feature = "terminal-tui")]
    pub(crate) fn enter() -> Result<Self> {
        let mut ops = CrosstermTerminalModeOps;
        let state = enter_terminal_state_with_ops(&mut ops)
            .map_err(|e| anyhow::anyhow!("failed to enter chat fullscreen TUI terminal mode: {e}"))?;
        Ok(Self {
            raw_mode_active: std::sync::atomic::AtomicBool::new(state.raw_mode_active),
            bracketed_paste_active: std::sync::atomic::AtomicBool::new(state.bracketed_paste_active),
            keyboard_enhancement_active: std::sync::atomic::AtomicBool::new(state.keyboard_enhancement_active),
            mouse_capture_active: std::sync::atomic::AtomicBool::new(state.mouse_capture_active),
            alternate_screen_active: std::sync::atomic::AtomicBool::new(state.alternate_screen_active),
        })
    }

    /// Manual early teardown (e.g. before spawning a child process that needs a
    /// clean terminal). Idempotent across manual calls and Drop.
    pub fn leave(&self) {
        let mut ops = CrosstermTerminalModeOps;
        let state = TerminalGuardState {
            bracketed_paste_active: self
                .bracketed_paste_active
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok(),
            mouse_capture_active: self
                .mouse_capture_active
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok(),
            keyboard_enhancement_active: self
                .keyboard_enhancement_active
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok(),
            alternate_screen_active: self
                .alternate_screen_active
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok(),
            raw_mode_active: self
                .raw_mode_active
                .compare_exchange(
                    true,
                    false,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok(),
        };
        if state.alternate_screen_active {
            CHAT_FULLSCREEN_ACTIVE.store(false, std::sync::atomic::Ordering::Release);
        }
        leave_terminal_state_with_ops(&mut ops, state);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
}

#[cfg(feature = "terminal-tui")]
pub(super) fn should_enable_terminal_tui(plain_mode: bool, stdin_is_terminal: bool, prx_tui_env: Option<&str>) -> bool {
    let tui_opt_out = prx_tui_env == Some("0");
    !plain_mode && !tui_opt_out && stdin_is_terminal
}

/// Install the chat-specific panic hook.
///
/// The hook restores the terminal (show cursor, leave alternate screen,
/// disable raw mode) **before** delegating to the previously installed hook
/// — so backtraces print to a usable terminal instead of a "bricked" raw-mode
/// alternate buffer.
///
/// A `OnceLock` guards against multiple concurrent panics each trying to run
/// the restoration sequence: only the first panic actually issues the
/// crossterm calls, subsequent panics skip straight to the chained backtrace
/// printer. This avoids fighting with `TerminalGuard::leave` (which may also
/// be running on the unwinding thread) and with each other.
///
/// Safe to call multiple times — only the first call installs the hook; later
/// calls return without rewrapping (avoids unbounded nesting and the original
/// hook being lost behind layers of restoration calls).
pub(super) fn install_chat_panic_hook() {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.set(()).is_err() {
        // Already installed by a previous call — do not stack another layer.
        return;
    }

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Defence in depth: even if a `TerminalGuard` is unwinding through
        // Drop on the panicking thread, this runs first (panic hook fires
        // before stack unwind) so the terminal is usable when the chained
        // hook prints the backtrace.
        //
        // A `OnceLock` ensures restoration runs at most once per process
        // even if multiple threads panic concurrently — preventing
        // interleaved escape sequences.
        static RESTORED: OnceLock<()> = OnceLock::new();
        if RESTORED.set(()).is_ok() {
            restore_terminal_state();
        }
        original_hook(info);
    }));
}

// ── P3-1: Redirect tracing to ~/.openprx/chat.log during chat ────────────

/// `main` redirects chat tracing before loading configuration so startup
/// diagnostics are not left behind the alternate screen and revealed on exit.
/// `chat::run` also calls `ensure_chat_tracing_to_file` for embedders that do
/// not enter through the binary bootstrap; this flag prevents a nested reload.
static CHAT_TRACING_TO_FILE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// RAII guard owning the `tracing_appender` non-blocking worker. When dropped
/// it:
///   1. swaps the global tracing writer back to stderr (best-effort), so
///      any post-chat logs (e.g. shutdown errors) remain visible;
///   2. drops `_worker_guard`, which flushes and joins the appender thread.
///
/// Held for the lifetime of `chat::run`. If construction fails we keep the
/// existing stderr writer — no panics, no silent data loss.
pub struct TracingChatGuard {
    _worker_guard: tracing_appender::non_blocking::WorkerGuard,
}

impl Drop for TracingChatGuard {
    fn drop(&mut self) {
        if let Some(handle) = crate::CHAT_TRACING_RELOAD.get() {
            let stderr_writer = tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr);
            let layer = tracing_subscriber::fmt::Layer::default().with_writer(stderr_writer);
            if let Err(e) = handle.reload(layer) {
                eprintln!("warning: failed to restore stderr tracing writer: {e}");
            }
        }
        CHAT_TRACING_TO_FILE_ACTIVE.store(false, Ordering::Release);
        // _worker_guard drops next → tracing-appender flushes pending lines.
    }
}

/// Chat log directory. An explicit `--config-dir` wins; otherwise the
/// historical default is `~/.openprx/` (or `$HOME/.openprx/`).
pub(super) fn resolve_chat_log_dir() -> Result<std::path::PathBuf> {
    if let Some(dir) = crate::config::process_config_dir() {
        return Ok(dir);
    }
    crate::config::default_openprx_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory for chat.log"))
}

/// Redirect the global `tracing` writer to `chat.log` so `INFO`/`WARN`/`ERROR`
/// lines never collide with the ratatui TUI. The file is under `--config-dir`
/// when that flag was published, and under `~/.openprx/` otherwise.
///
/// Returns a guard that MUST be held for the lifetime of the chat session.
/// On any failure (no HOME, dir not creatable, file not openable, no global
/// reload handle) returns `Err` without panicking — callers fall back to the
/// existing stderr writer.
pub fn setup_chat_tracing_to_file() -> Result<TracingChatGuard> {
    setup_chat_tracing_to_file_in(&resolve_chat_log_dir()?)
}

/// Test-friendly variant: redirects tracing to `<dir>/chat.log`. The directory
/// is created with `create_dir_all` if missing; the file is opened in append
/// mode so repeated chat invocations within one user session don't truncate
/// earlier logs.
pub(crate) fn setup_chat_tracing_to_file_in(dir: &std::path::Path) -> Result<TracingChatGuard> {
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("failed to create chat log directory {}: {e}", dir.display()))?;
    let log_path = dir.join("chat.log");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| anyhow::anyhow!("failed to open {} for writing: {e}", log_path.display()))?;

    let (non_blocking, worker_guard) = tracing_appender::non_blocking(file);
    let file_writer = tracing_subscriber::fmt::writer::BoxMakeWriter::new(non_blocking);
    // ANSI escape codes are useless (and noisy) inside a log file.
    let layer = tracing_subscriber::fmt::Layer::default()
        .with_writer(file_writer)
        .with_ansi(false);

    let handle = crate::CHAT_TRACING_RELOAD
        .get()
        .ok_or_else(|| anyhow::anyhow!("tracing reload handle not initialized (non-chat command?)"))?;
    handle
        .reload(layer)
        .map_err(|e| anyhow::anyhow!("failed to redirect tracing to chat.log: {e}"))?;

    CHAT_TRACING_TO_FILE_ACTIVE.store(true, Ordering::Release);

    Ok(TracingChatGuard {
        _worker_guard: worker_guard,
    })
}

/// Redirect tracing unless the binary bootstrap already owns the redirect.
pub(super) fn ensure_chat_tracing_to_file() -> Result<Option<TracingChatGuard>> {
    if CHAT_TRACING_TO_FILE_ACTIVE.load(Ordering::Acquire) {
        return Ok(None);
    }
    setup_chat_tracing_to_file().map(Some)
}
