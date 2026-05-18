//! Rich terminal channel with streaming output, Markdown rendering, and event-driven UI.
//!
//! `TerminalChannel` implements the [`Channel`] trait and routes all terminal I/O
//! through a single `UiActor` task to prevent output corruption from concurrent writes.
//! The existing `CliChannel` is preserved as a non-TTY / CI fallback.

#![allow(clippy::print_stdout, clippy::print_stderr)]

use super::traits::{Channel, ChannelCapabilities, ChannelMessage, SendMessage};
use anyhow::Result;
use async_trait::async_trait;
use crossterm::{execute, style, terminal};
use std::io::{self, IsTerminal as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;
use uuid::Uuid;

// ── Terminal output sanitization ────────────────────────────────────────────

/// Strip dangerous terminal escape sequences from untrusted text (LLM/tool output).
///
/// Removes all ANSI escape sequences (CSI, OSC, DCS, APC, PM, SOS) to prevent
/// clipboard manipulation, title injection, and other terminal escape attacks.
/// Preserves printable text plus \n, \r, \t.
fn sanitize_terminal_output(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => {
                // Consume the entire escape sequence
                match chars.peek() {
                    Some('[') => {
                        // CSI sequence: ESC [ ... final_byte (0x40-0x7E)
                        chars.next();
                        while let Some(&c) = chars.peek() {
                            chars.next();
                            if c.is_ascii() && (0x40..=0x7E).contains(&(c as u8)) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        // OSC sequence: ESC ] ... (ST or BEL)
                        chars.next();
                        while let Some(&c) = chars.peek() {
                            if c == '\x07' {
                                chars.next();
                                break;
                            }
                            if c == '\x1b' {
                                chars.next();
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                            chars.next();
                        }
                    }
                    Some('P' | 'X' | '^' | '_') => {
                        // DCS/SOS/PM/APC: ESC <intro> ... ST
                        chars.next();
                        while let Some(&c) = chars.peek() {
                            if c == '\x1b' {
                                chars.next();
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                            chars.next();
                        }
                    }
                    Some(_) => {
                        // Other ESC sequences: consume one more char
                        chars.next();
                    }
                    None => {}
                }
            }
            // Allow newline, carriage return, tab; strip other C0/C1 control chars
            '\n' | '\r' | '\t' => result.push(ch),
            c if c.is_control() => {}
            c => result.push(c),
        }
    }
    result
}

fn normalize_terminal_newlines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_was_cr = false;
    for ch in text.chars() {
        if ch == '\n' && !prev_was_cr {
            out.push('\r');
        }
        out.push(ch);
        prev_was_cr = ch == '\r';
    }
    out
}

fn print_terminal_text(text: &str) {
    print!("{}", normalize_terminal_newlines(text));
}

fn println_terminal_text(text: &str) {
    print_terminal_text(text);
    print_terminal_text("\n");
}

// ── UI Event types ──────────────────────────────────────────────────────────

/// All events that can affect terminal display.
/// Processed exclusively by the single [`UiActor`] task.
#[derive(Debug)]
enum UiEvent {
    // === Streaming events (with draft_id + seq for ordering) ===
    DraftStarted {
        draft_id: String,
    },
    TokenDelta {
        draft_id: String,
        seq: u64,
        text: String,
    },
    DraftFinalized {
        draft_id: String,
        full_text: String,
    },
    DraftCancelled {
        draft_id: String,
    },

    // === Tool events ===
    ToolCallStarted {
        name: String,
        args_summary: String,
    },
    ToolCallFinished {
        name: String,
        success: bool,
        duration_ms: u64,
    },
    ToolProgress {
        iteration: usize,
        max_iterations: usize,
    },

    // === Complete message (non-streaming fallback) ===
    FinalMessage {
        text: String,
    },

    // === Typing indicator ===
    TypingStart,
    TypingStop,

    // === Control events (highest priority) ===
    Shutdown,

    /// Install a [`TuiMirrorBridge`] on the running [`UiActor`].
    ///
    /// Sent once from [`TerminalChannel::with_tui_mirror`] after the actor has
    /// already been spawned. While the bridge is installed, every subsequent
    /// `UiEvent` is translated into a [`TuiState`] mutation + a non-blocking
    /// redraw kick instead of writing to stdout.
    ///
    /// Feature-gated because [`TuiState`] only exists with `terminal-tui`.
    #[cfg(feature = "terminal-tui")]
    AttachMirror {
        bridge: TuiMirrorBridge,
    },
}

// ── TUI mirror bridge ───────────────────────────────────────────────────────

/// Sink the [`UiActor`] talks to when it is in TUI translator mode.
///
/// `chat::tui::TuiState` lives in the binary crate (`main.rs`-rooted), so
/// the channels module — which lives in the library — cannot name it
/// directly. This trait is the seam: the binary implements it on top of its
/// own `TuiState` and hands a boxed instance to the actor via
/// [`TerminalChannel::with_tui_mirror`].
///
/// Each method corresponds to a single, already-sanitised translation of a
/// [`UiEvent`]. Implementations are expected to be cheap (push one
/// `ConversationLine`, hold the mutex briefly) and must never `.await` —
/// the actor invokes them synchronously inside its event loop.
#[cfg(feature = "terminal-tui")]
pub trait TuiMirrorSink: Send {
    /// Append an assistant message to the conversation.
    fn push_assistant(&self, content: &str);
    /// Append a dimmed system / status message to the conversation.
    fn push_system(&self, content: &str);
    /// Push a `Running` tool-result card.
    fn push_tool_started(&self, tool_name: &str, args_full: &str);
    /// Finalise the most recent `Running` card with the given name.
    /// Returns `true` if a matching card was found and updated.
    fn mark_tool_finished(&self, tool_name: &str, success: bool, duration_ms: u64) -> bool;

    // ── P3-5: streaming-draft surface ──────────────────────────────────
    //
    // The four methods below mirror `TuiState::{start,update,finalize,
    // cancel}_stream`. The default impls are intentionally no-ops so a
    // legacy sink (e.g. the in-test `RecordingSink`) that predates P3-5
    // still compiles — the binary's `TuiStateMirrorSink` overrides them
    // to drive the real ratatui frame.

    /// Begin a new streaming-assistant draft.
    fn start_stream(&self, _draft_id: &str) {}
    /// Replace the in-flight streaming draft's accumulated text.
    ///
    /// `accumulated` is the full running text so far (NOT a delta).
    /// `version` is monotonic; stale versions are dropped by the sink.
    fn update_stream(&self, _draft_id: &str, _accumulated: &str, _version: u64) {}
    /// Finalise the in-flight streaming draft with `final_text` and lift
    /// it into permanent conversation history.
    fn finalize_stream(&self, _draft_id: &str, _final_text: &str) {}
    /// Discard the in-flight streaming draft without surfacing any text.
    fn cancel_stream(&self, _draft_id: &str) {}
}

/// Routing handle that converts an actor running under [`UiActor::run`] from
/// a stdout printer into an event translator for the ratatui draw loop set
/// up by `chat::run` (P3-3).
///
/// Held in `Option<_>` so the same actor binary supports both rendering
/// modes:
///   * `None`  — legacy reedline path; events `print!` to stdout as before.
///   * `Some`  — TUI path; every event hits the trait sink (which usually
///     mutates a shared `TuiState` under a `parking_lot::Mutex`) and pokes
///     the render task via a 1-slot coalescing mpsc.
#[cfg(feature = "terminal-tui")]
pub struct TuiMirrorBridge {
    /// Sink that absorbs translated events. Concrete impl lives in the
    /// binary (`chat::tui`); the lib only sees the trait.
    pub sink: Box<dyn TuiMirrorSink>,
    /// 1-slot coalescing redraw signal. Producers use `try_send(())`; a full
    /// channel just means a redraw is already pending — drop silently.
    pub redraw_tx: mpsc::Sender<()>,
}

#[cfg(feature = "terminal-tui")]
impl std::fmt::Debug for TuiMirrorBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiMirrorBridge")
            .field("sink", &"<dyn TuiMirrorSink>")
            .field("redraw_tx", &"<mpsc::Sender<()>>")
            .finish()
    }
}

// ── UI State machine ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiState {
    /// Waiting for user input
    Idle,
    /// LLM is generating, streaming tokens
    Streaming,
    /// Final response being rendered
    Finalizing,
}

// ── UI Actor: sole owner of terminal output ─────────────────────────────────

/// Single task that owns all terminal writes.
/// Receives [`UiEvent`]s and renders them to stdout.
struct UiActor {
    state: UiState,
    event_rx: mpsc::Receiver<UiEvent>,
    /// Current draft being streamed
    active_draft_id: Option<String>,
    /// Accumulated text for the current streaming draft
    draft_buffer: String,
    /// Last sequence number processed (for ordering)
    last_seq: u64,
    /// Last repaint timestamp (for throttling)
    last_repaint: Instant,
    /// Repaint interval in milliseconds (~30fps)
    repaint_interval_ms: u64,
    /// Plain text mode (no ANSI escapes)
    plain_mode: bool,
    /// Cancel sender for the spinner animation task
    spinner_cancel: Option<tokio::sync::watch::Sender<bool>>,
    /// Optional TUI mirror bridge — when `Some`, the actor stops writing to
    /// stdout and instead translates every event into a [`TuiState`] mutation
    /// plus a coalesced redraw kick. Installed via the [`UiEvent::AttachMirror`]
    /// control event sent by [`TerminalChannel::with_tui_mirror`].
    #[cfg(feature = "terminal-tui")]
    mirror: Option<TuiMirrorBridge>,
}

impl UiActor {
    fn new(event_rx: mpsc::Receiver<UiEvent>, plain_mode: bool) -> Self {
        Self {
            state: UiState::Idle,
            event_rx,
            active_draft_id: None,
            draft_buffer: String::with_capacity(4096),
            last_seq: 0,
            last_repaint: Instant::now(),
            repaint_interval_ms: 33,
            plain_mode,
            spinner_cancel: None,
            #[cfg(feature = "terminal-tui")]
            mirror: None,
        }
    }

    /// Send a coalesced redraw signal to the ratatui render task.
    ///
    /// Uses `try_send` so a full 1-slot channel (redraw already pending) is
    /// not an error — that is the entire point of the coalescing design.
    /// A dropped receiver is also silent because the render task has simply
    /// exited (normal shutdown).
    #[cfg(feature = "terminal-tui")]
    fn notify_redraw(&self) {
        if let Some(bridge) = &self.mirror {
            let _ = bridge.redraw_tx.try_send(());
        }
    }

    /// Main event loop — runs until Shutdown event or channel close.
    async fn run(mut self) {
        while let Some(event) = self.event_rx.recv().await {
            match event {
                UiEvent::Shutdown => break,
                #[cfg(feature = "terminal-tui")]
                UiEvent::AttachMirror { bridge } => {
                    self.mirror = Some(bridge);
                }
                other => self.handle_event(other),
            }
        }
    }

    fn handle_event(&mut self, event: UiEvent) {
        // ── TUI mirror path ───────────────────────────────────────
        //
        // When a `TuiMirrorBridge` is installed, every event is translated
        // into a `TuiState` mutation + a coalesced redraw kick instead of
        // being written to stdout (which would tear the ratatui draw loop
        // from P3-3). The legacy print path below runs only when `mirror`
        // is `None`.
        //
        // ANSI sanitisation is preserved verbatim: untrusted LLM/tool text
        // is funnelled through `sanitize_terminal_output` before it lands
        // in `TuiState`, so a malicious provider response cannot inject
        // CSI/OSC escapes that the renderer would later repaint into the
        // frame buffer.
        #[cfg(feature = "terminal-tui")]
        if self.mirror.is_some() {
            self.handle_event_tui(event);
            return;
        }

        match event {
            UiEvent::DraftStarted { draft_id } => {
                self.state = UiState::Streaming;
                self.active_draft_id = Some(draft_id);
                self.draft_buffer.clear();
                self.last_seq = 0;
                // Start animated spinner (non-plain mode) or static indicator (plain mode)
                if !self.plain_mode {
                    let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
                    self.spinner_cancel = Some(cancel_tx);
                    tokio::spawn(async move {
                        let frames = [
                            '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}',
                            '\u{2827}', '\u{2807}', '\u{280F}',
                        ];
                        let mut idx: usize = 0;
                        loop {
                            tokio::select! {
                                () = tokio::time::sleep(std::time::Duration::from_millis(80)) => {
                                    if let Some(frame) = frames.get(idx % frames.len()) {
                                        eprint!("\r  {frame} Thinking...");
                                    }
                                    let _ = io::stderr().flush();
                                    idx = idx.wrapping_add(1);
                                }
                                _ = cancel_rx.changed() => {
                                    eprint!("\r                    \r");
                                    let _ = io::stderr().flush();
                                    break;
                                }
                            }
                        }
                    });
                }
            }

            UiEvent::TokenDelta { draft_id, seq, text } => {
                // Ignore events for stale drafts or out-of-order events
                if self.state != UiState::Streaming {
                    return;
                }
                if self.active_draft_id.as_deref() != Some(&draft_id) {
                    return;
                }
                if seq <= self.last_seq {
                    return;
                }
                self.last_seq = seq;

                // On first token, cancel spinner and clear the line
                if self.draft_buffer.is_empty() {
                    if let Some(cancel) = self.spinner_cancel.take() {
                        let _ = cancel.send(true);
                    }
                    print!("\r");
                    let _ = execute!(io::stdout(), terminal::Clear(terminal::ClearType::CurrentLine));
                }

                // Compute delta from accumulated text (safe char-boundary slicing)
                let new_content =
                    if text.len() > self.draft_buffer.len() && text.is_char_boundary(self.draft_buffer.len()) {
                        &text[self.draft_buffer.len()..]
                    } else if text.len() > self.draft_buffer.len() {
                        // Byte lengths diverged at a multi-byte char boundary —
                        // fall back to printing full accumulated text next repaint
                        &text
                    } else {
                        &text
                    };

                // Sanitize untrusted LLM output before printing
                let safe_content = sanitize_terminal_output(new_content);

                // Throttle repaints
                let elapsed = self.last_repaint.elapsed().as_millis() as u64;
                if elapsed >= self.repaint_interval_ms || safe_content.contains('\n') {
                    print_terminal_text(&safe_content);
                    let _ = io::stdout().flush();
                    self.last_repaint = Instant::now();
                }

                self.draft_buffer = text;
            }

            UiEvent::DraftFinalized { draft_id, full_text } => {
                // Cancel spinner if still running
                if let Some(cancel) = self.spinner_cancel.take() {
                    let _ = cancel.send(true);
                }

                if self.active_draft_id.as_deref() != Some(&draft_id) {
                    // Stale finalize — just print as a final message
                    if !full_text.is_empty() {
                        let safe = sanitize_terminal_output(&full_text);
                        print_terminal_text("\n");
                        println_terminal_text(&safe);
                        print_terminal_text("\n");
                    }
                    return;
                }

                self.state = UiState::Finalizing;

                // Flush any remaining un-rendered delta (safe char-boundary + sanitize)
                if full_text.len() > self.draft_buffer.len() && full_text.is_char_boundary(self.draft_buffer.len()) {
                    let remaining = sanitize_terminal_output(&full_text[self.draft_buffer.len()..]);
                    print_terminal_text(&remaining);
                }
                print_terminal_text("\n\n");
                let _ = io::stdout().flush();

                self.draft_buffer.clear();
                self.active_draft_id = None;
                self.state = UiState::Idle;
            }

            UiEvent::DraftCancelled { draft_id } => {
                // Cancel spinner if still running
                if let Some(cancel) = self.spinner_cancel.take() {
                    let _ = cancel.send(true);
                }

                if self.active_draft_id.as_deref() == Some(&draft_id) {
                    // Clear current line
                    print!("\r");
                    let _ = execute!(io::stdout(), terminal::Clear(terminal::ClearType::CurrentLine));
                    if !self.draft_buffer.is_empty() {
                        print_terminal_text("\n");
                        if !self.plain_mode {
                            println_terminal_text("  (cancelled)");
                        }
                    }
                    self.draft_buffer.clear();
                    self.active_draft_id = None;
                    self.state = UiState::Idle;
                }
            }

            UiEvent::ToolCallStarted { name, args_summary } => {
                if self.plain_mode {
                    println!("  [tool: {name}] {args_summary}");
                } else {
                    let _ = execute!(
                        io::stdout(),
                        style::SetForegroundColor(style::Color::Yellow),
                        style::Print(format!("  ▶ {name}")),
                        style::ResetColor,
                        style::SetForegroundColor(style::Color::DarkGrey),
                        style::Print(format!(" {args_summary}\n")),
                        style::ResetColor
                    );
                }
            }

            UiEvent::ToolCallFinished {
                name,
                success,
                duration_ms,
            } => {
                let duration_str = if duration_ms >= 1000 {
                    format!("{:.1}s", duration_ms as f64 / 1000.0)
                } else {
                    format!("{duration_ms}ms")
                };
                if success {
                    if !self.plain_mode {
                        let _ = execute!(
                            io::stdout(),
                            style::SetForegroundColor(style::Color::Green),
                            style::Print(format!("  \u{2713} {name} ({duration_str})\n")),
                            style::ResetColor
                        );
                    }
                } else if !self.plain_mode {
                    let _ = execute!(
                        io::stdout(),
                        style::SetForegroundColor(style::Color::Red),
                        style::Print(format!("  \u{2717} {name} (failed, {duration_str})\n")),
                        style::ResetColor
                    );
                } else {
                    let status = if success { "ok" } else { "failed" };
                    println!("  [tool: {name}] {status} ({duration_str})");
                }
            }

            UiEvent::ToolProgress {
                iteration,
                max_iterations,
            } => {
                if !self.plain_mode {
                    let _ = execute!(
                        io::stdout(),
                        style::SetForegroundColor(style::Color::Cyan),
                        style::Print(format!(
                            "  \u{25B6} Step {iteration}/{max_iterations} \u{2014} continuing...\n"
                        )),
                        style::ResetColor
                    );
                } else {
                    println!("  [step {iteration}/{max_iterations}] continuing...");
                }
            }

            UiEvent::FinalMessage { text } => {
                if !text.is_empty() {
                    let safe = sanitize_terminal_output(&text);
                    print_terminal_text("\n");
                    println_terminal_text(&safe);
                    print_terminal_text("\n");
                }
            }

            UiEvent::TypingStart => {
                if !self.plain_mode && self.state == UiState::Idle {
                    let _ = execute!(
                        io::stdout(),
                        style::SetForegroundColor(style::Color::DarkGrey),
                        style::Print("  Thinking..."),
                        style::ResetColor
                    );
                    let _ = io::stdout().flush();
                }
            }

            UiEvent::TypingStop => {
                print!("\r");
                let _ = execute!(io::stdout(), terminal::Clear(terminal::ClearType::CurrentLine));
                let _ = io::stdout().flush();
            }

            UiEvent::Shutdown => {
                // Handled in run() loop
            }

            #[cfg(feature = "terminal-tui")]
            UiEvent::AttachMirror { .. } => {
                // Already intercepted in `run()`. Reaching this arm would be
                // a state-machine bug; silent drop is the safe response.
            }
        }
    }

    /// Translate a [`UiEvent`] into [`TuiState`] mutations plus a coalesced
    /// redraw signal, replacing the legacy stdout writes (P3-4).
    ///
    /// Called only while `self.mirror.is_some()`. Every textual payload from
    /// untrusted sources (LLM, tool args, tool results) is first run through
    /// [`sanitize_terminal_output`] so the renderer cannot repaint a CSI or
    /// OSC escape from a malicious provider response into the frame.
    ///
    /// `DraftStarted` opens a streaming slot on the sink (P3-5); subsequent
    /// `TokenDelta` events drive `update_stream` with the running accumulated
    /// text. `DraftFinalized` lifts the buffer into permanent history.
    /// `DraftCancelled` discards the slot without surfacing any text.
    #[cfg(feature = "terminal-tui")]
    fn handle_event_tui(&mut self, event: UiEvent) {
        // Every branch below acquires the mirror lock for the shortest
        // possible window (one push, then drop). The mutex is parking_lot,
        // so no `.await` may appear inside its scope — and none does.
        let Some(bridge) = self.mirror.as_ref() else {
            // Defensive: caller guards with `mirror.is_some()`. If we ever
            // reach here, dropping the event is strictly safer than panicking.
            return;
        };

        match event {
            UiEvent::DraftStarted { draft_id } => {
                self.state = UiState::Streaming;
                self.active_draft_id = Some(draft_id.clone());
                self.draft_buffer.clear();
                self.last_seq = 0;
                // P3-5: open a streaming-assistant slot on the TUI sink so
                // subsequent TokenDelta events have something to mutate.
                bridge.sink.start_stream(&draft_id);
                // No spinner under TUI; the render task draws its own status
                // bar. Still notify so any state-derived indicator refreshes.
                self.notify_redraw();
            }

            UiEvent::TokenDelta { draft_id, seq, text } => {
                // P3-5: drive the streaming-assistant frame block. `text`
                // is already the full accumulated text from the channel
                // layer (see `TerminalChannel::update_draft`), so we don't
                // need to maintain a per-draft accumulator here — `seq`
                // doubles as the monotonic version for stale-delta drop.
                if self.state != UiState::Streaming {
                    return;
                }
                if self.active_draft_id.as_deref() != Some(&draft_id) {
                    return;
                }
                if seq <= self.last_seq {
                    // Stale / out-of-order delta — drop silently.
                    return;
                }
                self.last_seq = seq;
                // Sanitise before the bytes reach the renderer so a CSI
                // injection from a malicious LLM provider cannot ride a
                // delta into the frame buffer.
                let safe = sanitize_terminal_output(&text);
                bridge.sink.update_stream(&draft_id, &safe, seq);
                self.draft_buffer.clear();
                self.draft_buffer.push_str(&text);
                self.notify_redraw();
            }

            UiEvent::DraftFinalized { draft_id, full_text } => {
                let is_active = self.active_draft_id.as_deref() == Some(&draft_id);
                self.state = UiState::Idle;
                if is_active {
                    self.active_draft_id = None;
                    self.draft_buffer.clear();
                    self.last_seq = 0;
                }
                // P3-5: hand the final text to the sink so it can lift the
                // streaming buffer into permanent history. Even an empty
                // finalisation must clear the streaming slot — otherwise a
                // cancelled-then-zero-finalised draft would linger.
                let safe = sanitize_terminal_output(&full_text);
                bridge.sink.finalize_stream(&draft_id, &safe);
                self.notify_redraw();
            }

            UiEvent::DraftCancelled { draft_id } => {
                if self.active_draft_id.as_deref() == Some(&draft_id) {
                    self.active_draft_id = None;
                    self.draft_buffer.clear();
                    self.last_seq = 0;
                    self.state = UiState::Idle;
                    // P3-5: drop the streaming buffer first, then surface a
                    // dimmed system note so the user sees the cancellation.
                    bridge.sink.cancel_stream(&draft_id);
                    bridge.sink.push_system("(cancelled)");
                    self.notify_redraw();
                }
            }

            UiEvent::ToolCallStarted { name, args_summary } => {
                // Tool args are echoed verbatim from the tool layer; sanitise
                // both name and args before they reach the renderer.
                let safe_name = sanitize_terminal_output(&name);
                let safe_args = sanitize_terminal_output(&args_summary);
                bridge.sink.push_tool_started(&safe_name, &safe_args);
                self.notify_redraw();
            }

            UiEvent::ToolCallFinished {
                name,
                success,
                duration_ms,
            } => {
                let safe_name = sanitize_terminal_output(&name);
                // The tool-event forwarder in `chat::run` already mirrors
                // tool start/finish events into the same `TuiState` (P2-7).
                // The UiActor mirror call is a defensive duplicate: if a
                // matching `Running` card exists we mark it Done/Error,
                // otherwise the no-op return value is silently ignored.
                let _updated = bridge.sink.mark_tool_finished(&safe_name, success, duration_ms);
                self.notify_redraw();
            }

            UiEvent::ToolProgress {
                iteration,
                max_iterations,
            } => {
                let msg = format!("step {iteration}/{max_iterations}");
                bridge.sink.push_system(&msg);
                self.notify_redraw();
            }

            UiEvent::FinalMessage { text } => {
                if text.is_empty() {
                    return;
                }
                let safe = sanitize_terminal_output(&text);
                bridge.sink.push_assistant(&safe);
                self.notify_redraw();
            }

            UiEvent::TypingStart | UiEvent::TypingStop => {
                // The TUI draws its own typing indicator from `TuiState`.
                // The legacy event is a no-op here; mapping it to a
                // dedicated `is_typing` flag is tracked under P3-5.
            }

            UiEvent::Shutdown | UiEvent::AttachMirror { .. } => {
                // Both are intercepted in `run()`; reaching here would be a
                // state-machine bug. Silent drop is the safe response.
            }
        }
    }
}

// ── TerminalChannel ─────────────────────────────────────────────────────────

/// Rich terminal channel with streaming output.
///
/// All terminal writes are serialized through a single [`UiActor`] task.
/// The `Channel` trait methods send [`UiEvent`]s via an mpsc sender.
pub struct TerminalChannel {
    /// Sender to the UI Actor — all terminal output goes through this
    ui_tx: mpsc::Sender<UiEvent>,
    /// Monotonic sequence counter for draft token ordering
    seq_counter: AtomicU64,
    /// Currently active draft ID (for update_draft → TokenDelta mapping)
    active_draft_id: Arc<parking_lot::Mutex<Option<String>>>,
}

impl TerminalChannel {
    /// Create a new `TerminalChannel` and spawn the UI Actor task.
    ///
    /// Returns the channel and a handle to the UI Actor task.
    pub fn new(plain_mode: bool) -> Self {
        // Bounded channel: control events are never coalesced, only TokenDelta can be
        let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(128);

        let actor = UiActor::new(ui_rx, plain_mode);
        tokio::spawn(actor.run());

        Self {
            ui_tx,
            seq_counter: AtomicU64::new(0),
            active_draft_id: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// Attach a [`TuiMirrorBridge`] so the running [`UiActor`] stops writing
    /// to stdout and instead translates every event into a [`TuiState`]
    /// mutation + a coalesced redraw kick (P3-4).
    ///
    /// Should be called immediately after [`Self::new`] and before any
    /// `UiEvent` is sent (i.e. before `listen()`, before
    /// `send_draft`/`send`/tool-event mirror). Internally this issues a
    /// one-shot [`UiEvent::AttachMirror`] control event; once delivered the
    /// actor swaps the bridge in place and all subsequent events follow the
    /// TUI translator path. Callers that never invoke this method keep the
    /// legacy stdout path intact, preserving the non-TUI / CI fallback.
    ///
    /// On send failure (channel closed because the actor already exited)
    /// the call is silently best-effort: the only realistic cause is
    /// shutdown, where no further events will flow anyway.
    #[cfg(feature = "terminal-tui")]
    pub async fn with_tui_mirror(&self, sink: Box<dyn TuiMirrorSink>, redraw_tx: mpsc::Sender<()>) {
        let _ = self
            .ui_tx
            .send(UiEvent::AttachMirror {
                bridge: TuiMirrorBridge { sink, redraw_tx },
            })
            .await;
    }
}

#[async_trait]
impl Channel for TerminalChannel {
    fn name(&self) -> &str {
        "terminal"
    }

    fn supports_draft_updates(&self) -> bool {
        true
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            edit: false,
            delete: false,
            thread: false,
            react: false,
        }
    }

    async fn send(&self, message: &SendMessage) -> Result<()> {
        let _ = self
            .ui_tx
            .send(UiEvent::FinalMessage {
                text: message.content.clone(),
            })
            .await;
        Ok(())
    }

    async fn send_draft(&self, _message: &SendMessage) -> Result<Option<String>> {
        let draft_id = Uuid::new_v4().to_string();
        self.seq_counter.store(0, Ordering::Relaxed);
        *self.active_draft_id.lock() = Some(draft_id.clone());

        let _ = self
            .ui_tx
            .send(UiEvent::DraftStarted {
                draft_id: draft_id.clone(),
            })
            .await;
        Ok(Some(draft_id))
    }

    async fn update_draft(&self, _recipient: &str, message_id: &str, text: &str) -> Result<()> {
        // Use the caller-provided message_id; verify it matches the active draft
        let active = self.active_draft_id.lock().clone();
        if active.as_deref() == Some(message_id) {
            let seq = self.seq_counter.fetch_add(1, Ordering::Relaxed) + 1;
            let _ = self
                .ui_tx
                .send(UiEvent::TokenDelta {
                    draft_id: message_id.to_string(),
                    seq,
                    text: text.to_string(),
                })
                .await;
        }
        Ok(())
    }

    async fn finalize_draft(&self, _recipient: &str, message_id: &str, text: &str) -> Result<()> {
        // Check + clear under lock, then send without holding the guard
        let matched = {
            let mut active = self.active_draft_id.lock();
            if active.as_deref() == Some(message_id) {
                *active = None;
                true
            } else {
                false
            }
        };
        if matched {
            let _ = self
                .ui_tx
                .send(UiEvent::DraftFinalized {
                    draft_id: message_id.to_string(),
                    full_text: text.to_string(),
                })
                .await;
        }
        Ok(())
    }

    async fn cancel_draft(&self, _recipient: &str, message_id: &str) -> Result<()> {
        let matched = {
            let mut active = self.active_draft_id.lock();
            if active.as_deref() == Some(message_id) {
                *active = None;
                true
            } else {
                false
            }
        };
        if matched {
            let _ = self
                .ui_tx
                .send(UiEvent::DraftCancelled {
                    draft_id: message_id.to_string(),
                })
                .await;
        }
        Ok(())
    }

    async fn start_typing(&self, _recipient: &str) -> Result<()> {
        let _ = self.ui_tx.send(UiEvent::TypingStart).await;
        Ok(())
    }

    async fn stop_typing(&self, _recipient: &str) -> Result<()> {
        let _ = self.ui_tx.send(UiEvent::TypingStop).await;
        Ok(())
    }

    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> Result<()> {
        terminal_input_loop(tx).await
    }
}

impl TerminalChannel {
    /// Send a tool-call-started notification to the UI Actor.
    pub async fn notify_tool_started(&self, name: &str, args_summary: &str) {
        let _ = self
            .ui_tx
            .send(UiEvent::ToolCallStarted {
                name: name.to_string(),
                args_summary: args_summary.to_string(),
            })
            .await;
    }

    /// Send a tool-call-finished notification to the UI Actor.
    pub async fn notify_tool_finished(&self, name: &str, success: bool, duration_ms: u64) {
        let _ = self
            .ui_tx
            .send(UiEvent::ToolCallFinished {
                name: name.to_string(),
                success,
                duration_ms,
            })
            .await;
    }

    /// Send a tool-loop progress notification to the UI Actor.
    pub async fn notify_progress(&self, iteration: usize, max_iterations: usize) {
        let _ = self
            .ui_tx
            .send(UiEvent::ToolProgress {
                iteration,
                max_iterations,
            })
            .await;
    }
}

impl Drop for TerminalChannel {
    fn drop(&mut self) {
        // Best-effort shutdown signal
        let _ = self.ui_tx.try_send(UiEvent::Shutdown);
    }
}

// ── Input loop ──────────────────────────────────────────────────────────────

/// Interactive input loop.
///
/// Reads user input, recognizes slash-commands, and sends `ChannelMessage`s
/// to the agent pipeline.
async fn terminal_input_loop(tx: mpsc::Sender<ChannelMessage>) -> Result<()> {
    // Run stdin reads on a blocking thread. The legacy fallback deliberately
    // uses a simple prompt instead of reedline: reedline consumes Ctrl+C and
    // redraws over concurrent chat output unless wired through its optional
    // external-printer feature.
    let handle = tokio::task::spawn_blocking(move || -> Result<()> {
        use std::io::BufRead as _;

        let stdin_is_tty = io::stdin().is_terminal();
        let stdin = io::stdin();
        let mut lines = stdin.lock().lines();
        loop {
            if stdin_is_tty {
                print!("prx〉");
                let _ = io::stdout().flush();
            }
            let line = match lines.next() {
                Some(Ok(line)) => line,
                Some(Err(_)) | None => break,
            };
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "/quit" || trimmed == "/exit" {
                break;
            }

            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let msg = ChannelMessage {
                id: Uuid::new_v4().to_string(),
                sender: "user".to_string(),
                reply_target: "user".to_string(),
                content: trimmed,
                channel: "terminal".to_string(),
                timestamp,
                thread_ts: None,
                mentioned_uuids: vec![],
            };

            if tx.blocking_send(msg).is_err() {
                break;
            }
        }
        Ok(())
    });

    handle.await??;
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_state_transitions() {
        assert_eq!(UiState::Idle, UiState::Idle);
        assert_ne!(UiState::Idle, UiState::Streaming);
        assert_ne!(UiState::Streaming, UiState::Finalizing);
    }

    #[test]
    fn ui_event_debug_format() {
        let event = UiEvent::DraftStarted {
            draft_id: "test-123".to_string(),
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("DraftStarted"));
        assert!(debug.contains("test-123"));
    }

    #[test]
    fn terminal_output_newlines_use_carriage_return() {
        assert_eq!(normalize_terminal_newlines("a\nb"), "a\r\nb");
        assert_eq!(normalize_terminal_newlines("a\r\nb"), "a\r\nb");
        assert_eq!(normalize_terminal_newlines("a\nb\n"), "a\r\nb\r\n");
    }

    #[tokio::test]
    async fn terminal_channel_name() {
        let ch = TerminalChannel::new(true);
        assert_eq!(ch.name(), "terminal");
    }

    #[tokio::test]
    async fn terminal_channel_supports_drafts() {
        let ch = TerminalChannel::new(true);
        assert!(ch.supports_draft_updates());
    }

    #[tokio::test]
    async fn send_draft_returns_id() {
        let ch = TerminalChannel::new(true);
        let result = ch.send_draft(&SendMessage::new("test", "user")).await;
        assert!(result.is_ok());
        let draft_id = result.unwrap();
        assert!(draft_id.is_some());
        assert!(!draft_id.unwrap().is_empty());
    }

    #[tokio::test]
    async fn update_and_finalize_draft() {
        let ch = TerminalChannel::new(true);
        let draft_id = ch.send_draft(&SendMessage::new("", "user")).await.unwrap().unwrap();
        ch.update_draft("user", &draft_id, "Hello").await.unwrap();
        ch.update_draft("user", &draft_id, "Hello world").await.unwrap();
        ch.finalize_draft("user", &draft_id, "Hello world").await.unwrap();
    }

    #[tokio::test]
    async fn cancel_draft_succeeds() {
        let ch = TerminalChannel::new(true);
        let draft_id = ch.send_draft(&SendMessage::new("", "user")).await.unwrap().unwrap();
        ch.update_draft("user", &draft_id, "partial").await.unwrap();
        ch.cancel_draft("user", &draft_id).await.unwrap();
    }

    #[tokio::test]
    async fn finalize_after_cancel_is_safe() {
        let ch = TerminalChannel::new(true);
        let draft_id = ch.send_draft(&SendMessage::new("", "user")).await.unwrap().unwrap();
        ch.cancel_draft("user", &draft_id).await.unwrap();
        // finalize on already-cancelled draft should not panic
        ch.finalize_draft("user", &draft_id, "late text").await.unwrap();
    }

    #[tokio::test]
    async fn send_final_message() {
        let ch = TerminalChannel::new(true);
        let result = ch.send(&SendMessage::new("Hello user", "user")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn capabilities_reported_correctly() {
        let ch = TerminalChannel::new(true);
        let caps = ch.capabilities();
        assert!(!caps.edit);
        assert!(!caps.delete);
        assert!(!caps.thread);
        assert!(!caps.react);
    }

    #[tokio::test]
    async fn typing_indicators_do_not_error() {
        let ch = TerminalChannel::new(true);
        assert!(ch.start_typing("user").await.is_ok());
        assert!(ch.stop_typing("user").await.is_ok());
    }

    #[test]
    fn seq_counter_monotonic() {
        let counter = AtomicU64::new(0);
        let a = counter.fetch_add(1, Ordering::Relaxed) + 1;
        let b = counter.fetch_add(1, Ordering::Relaxed) + 1;
        let c = counter.fetch_add(1, Ordering::Relaxed) + 1;
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(c, 3);
    }

    #[test]
    fn sanitize_strips_csi_sequences() {
        let malicious = "Hello \x1b[31mred\x1b[0m world";
        let result = sanitize_terminal_output(malicious);
        assert_eq!(result, "Hello red world");
    }

    #[test]
    fn sanitize_strips_osc_clipboard_attack() {
        // OSC 52 clipboard injection attempt
        let attack = "normal\x1b]52;c;SGVsbG8=\x07text";
        let result = sanitize_terminal_output(attack);
        assert_eq!(result, "normaltext");
    }

    #[test]
    fn sanitize_strips_title_injection() {
        // OSC 0/2 title change attempt
        let attack = "safe\x1b]0;malicious-title\x07rest";
        let result = sanitize_terminal_output(attack);
        assert_eq!(result, "saferest");
    }

    #[test]
    fn sanitize_preserves_normal_text() {
        let normal = "Hello, 你好世界! Line\nnewline\ttab";
        let result = sanitize_terminal_output(normal);
        assert_eq!(result, normal);
    }

    #[test]
    fn sanitize_strips_control_chars() {
        let with_controls = "hello\x00\x01\x02world";
        let result = sanitize_terminal_output(with_controls);
        assert_eq!(result, "helloworld");
    }

    // ── P3-4: TUI mirror routing tests ──────────────────────────────────────

    /// In-memory implementation of [`TuiMirrorSink`] used by the routing
    /// tests below. Captures every sink invocation so we can assert on
    /// translation order, payload sanitisation, and tool-card transitions
    /// without depending on the real `chat::tui::TuiState` (which lives in
    /// the binary crate and is not reachable from `crate::channels`).
    #[cfg(feature = "terminal-tui")]
    #[derive(Default)]
    struct RecordingSink {
        events: parking_lot::Mutex<Vec<RecordedEvent>>,
    }

    /// Single recorded sink call, tagged by translation kind.
    #[cfg(feature = "terminal-tui")]
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum RecordedEvent {
        Assistant(String),
        System(String),
        ToolStarted {
            name: String,
            args: String,
        },
        ToolFinished {
            name: String,
            success: bool,
            duration_ms: u64,
        },
        // P3-5: streaming-draft surface
        StreamStarted {
            draft_id: String,
        },
        StreamUpdated {
            draft_id: String,
            accumulated: String,
            version: u64,
        },
        StreamFinalized {
            draft_id: String,
            final_text: String,
        },
        StreamCancelled {
            draft_id: String,
        },
    }

    #[cfg(feature = "terminal-tui")]
    impl RecordingSink {
        fn snapshot(&self) -> Vec<RecordedEvent> {
            self.events.lock().clone()
        }
    }

    #[cfg(feature = "terminal-tui")]
    impl TuiMirrorSink for Arc<RecordingSink> {
        fn push_assistant(&self, content: &str) {
            self.events.lock().push(RecordedEvent::Assistant(content.to_string()));
        }
        fn push_system(&self, content: &str) {
            self.events.lock().push(RecordedEvent::System(content.to_string()));
        }
        fn push_tool_started(&self, tool_name: &str, args_full: &str) {
            self.events.lock().push(RecordedEvent::ToolStarted {
                name: tool_name.to_string(),
                args: args_full.to_string(),
            });
        }
        fn mark_tool_finished(&self, tool_name: &str, success: bool, duration_ms: u64) -> bool {
            self.events.lock().push(RecordedEvent::ToolFinished {
                name: tool_name.to_string(),
                success,
                duration_ms,
            });
            true
        }
        // P3-5: streaming bridge
        fn start_stream(&self, draft_id: &str) {
            self.events.lock().push(RecordedEvent::StreamStarted {
                draft_id: draft_id.to_string(),
            });
        }
        fn update_stream(&self, draft_id: &str, accumulated: &str, version: u64) {
            self.events.lock().push(RecordedEvent::StreamUpdated {
                draft_id: draft_id.to_string(),
                accumulated: accumulated.to_string(),
                version,
            });
        }
        fn finalize_stream(&self, draft_id: &str, final_text: &str) {
            self.events.lock().push(RecordedEvent::StreamFinalized {
                draft_id: draft_id.to_string(),
                final_text: final_text.to_string(),
            });
        }
        fn cancel_stream(&self, draft_id: &str) {
            self.events.lock().push(RecordedEvent::StreamCancelled {
                draft_id: draft_id.to_string(),
            });
        }
    }

    /// Wait up to `tries` * 10ms for `pred` to hold over the captured sink
    /// snapshot. The actor task is `tokio::spawn`ed, so even after
    /// `ch.send().await` returns we may not have observed the translation
    /// yet — this is the standard "give the actor a tick" idiom.
    #[cfg(feature = "terminal-tui")]
    async fn wait_for_sink(sink: &Arc<RecordingSink>, tries: u32, pred: impl Fn(&[RecordedEvent]) -> bool) -> bool {
        for _ in 0..tries {
            if pred(&sink.snapshot()) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        pred(&sink.snapshot())
    }

    /// After `with_tui_mirror`, `send()` should land an `Assistant` line in
    /// the shared sink and emit a redraw signal on the coalescing channel —
    /// instead of writing to stdout.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn tui_mirror_routes_send_to_assistant_line() {
        let sink = Arc::new(RecordingSink::default());
        let (redraw_tx, mut redraw_rx) = mpsc::channel::<()>(1);
        let ch = TerminalChannel::new(true);
        ch.with_tui_mirror(Box::new(Arc::clone(&sink)), redraw_tx).await;

        ch.send(&SendMessage::new("hello world", "user"))
            .await
            .expect("test: send succeeds");

        assert!(
            wait_for_sink(&sink, 50, |evts| {
                evts.iter()
                    .any(|e| matches!(e, RecordedEvent::Assistant(c) if c == "hello world"))
            })
            .await,
            "expected Assistant translation; got {:?}",
            sink.snapshot()
        );
        assert!(redraw_rx.try_recv().is_ok(), "redraw signal should be pending");
    }

    /// P3-5: `send_draft` + `update_draft` + `finalize_draft` under the TUI
    /// path should land Stream{Started,Updated,Finalized} translations in
    /// order, and MUST NOT use the legacy `push_assistant` surface (which is
    /// reserved for non-streaming `FinalMessage` events).
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn tui_mirror_finalize_draft_pushes_assistant() {
        let sink = Arc::new(RecordingSink::default());
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let ch = TerminalChannel::new(true);
        ch.with_tui_mirror(Box::new(Arc::clone(&sink)), redraw_tx).await;

        let draft_id = ch
            .send_draft(&SendMessage::new("", "user"))
            .await
            .expect("test: send_draft ok")
            .expect("test: draft id");
        ch.update_draft("user", &draft_id, "partial")
            .await
            .expect("test: update_draft ok");
        ch.finalize_draft("user", &draft_id, "complete answer")
            .await
            .expect("test: finalize ok");

        assert!(
            wait_for_sink(&sink, 50, |evts| {
                evts.iter().any(|e| {
                    matches!(
                        e,
                        RecordedEvent::StreamFinalized { final_text, .. } if final_text == "complete answer"
                    )
                })
            })
            .await,
            "expected StreamFinalized('complete answer'); got {:?}",
            sink.snapshot()
        );
        // The streaming path must NEVER push through the legacy
        // `push_assistant` surface — that is reserved for non-streaming
        // `FinalMessage` events.
        let assistants: Vec<_> = sink
            .snapshot()
            .into_iter()
            .filter(|e| matches!(e, RecordedEvent::Assistant(_)))
            .collect();
        assert!(
            assistants.is_empty(),
            "TokenDelta path must not emit Assistant; got {assistants:?}"
        );
    }

    /// P3-5: a streaming sequence (`send_draft` → `update_draft` × 2 →
    /// `finalize_draft`) translates into ordered Stream{Started,Updated,
    /// Finalized} sink calls carrying the accumulated text and monotonic
    /// version (`seq`).
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn tui_mirror_token_delta_drives_update_stream() {
        let sink = Arc::new(RecordingSink::default());
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let ch = TerminalChannel::new(true);
        ch.with_tui_mirror(Box::new(Arc::clone(&sink)), redraw_tx).await;

        let draft_id = ch
            .send_draft(&SendMessage::new("", "user"))
            .await
            .expect("test: send_draft ok")
            .expect("test: draft id");
        ch.update_draft("user", &draft_id, "Hel").await.expect("test: u1");
        ch.update_draft("user", &draft_id, "Hello").await.expect("test: u2");
        ch.finalize_draft("user", &draft_id, "Hello world")
            .await
            .expect("test: finalize ok");

        assert!(
            wait_for_sink(&sink, 50, |evts| {
                evts.iter().any(|e| {
                    matches!(
                        e,
                        RecordedEvent::StreamFinalized { final_text, .. } if final_text == "Hello world"
                    )
                })
            })
            .await,
            "expected StreamFinalized after sequence; got {:?}",
            sink.snapshot()
        );

        let snap = sink.snapshot();
        let started = snap
            .iter()
            .position(|e| matches!(e, RecordedEvent::StreamStarted { draft_id: d } if d == &draft_id))
            .expect("test: StreamStarted present");
        let updates: Vec<(String, u64)> = snap
            .iter()
            .filter_map(|e| match e {
                RecordedEvent::StreamUpdated {
                    draft_id: d,
                    accumulated,
                    version,
                } if d == &draft_id => Some((accumulated.clone(), *version)),
                _ => None,
            })
            .collect();
        let finalized = snap
            .iter()
            .position(|e| matches!(e, RecordedEvent::StreamFinalized { draft_id: d, .. } if d == &draft_id))
            .expect("test: StreamFinalized present");

        assert!(started < finalized, "Started before Finalized");
        assert_eq!(updates.len(), 2, "two TokenDelta translations; got {updates:?}");
        let u0 = updates.first().expect("test: first update");
        let u1 = updates.get(1).expect("test: second update");
        assert_eq!(u0.0, "Hel");
        assert_eq!(u1.0, "Hello");
        // seq is monotonic (TerminalChannel::update_draft uses fetch_add(1)+1).
        assert!(u0.1 < u1.1, "version strictly increases");
    }

    /// P3-5: an in-flight draft cancellation must translate into a
    /// `StreamCancelled` call (not a `StreamFinalized`), and surface the
    /// `(cancelled)` system note for user feedback.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn tui_mirror_cancel_draft_emits_stream_cancel() {
        let sink = Arc::new(RecordingSink::default());
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let ch = TerminalChannel::new(true);
        ch.with_tui_mirror(Box::new(Arc::clone(&sink)), redraw_tx).await;

        let draft_id = ch
            .send_draft(&SendMessage::new("", "user"))
            .await
            .expect("test: send_draft ok")
            .expect("test: draft id");
        ch.update_draft("user", &draft_id, "partial")
            .await
            .expect("test: update ok");
        ch.cancel_draft("user", &draft_id).await.expect("test: cancel ok");

        assert!(
            wait_for_sink(&sink, 50, |evts| {
                evts.iter()
                    .any(|e| matches!(e, RecordedEvent::StreamCancelled { draft_id: d } if d == &draft_id))
            })
            .await,
            "expected StreamCancelled; got {:?}",
            sink.snapshot()
        );
        let snap = sink.snapshot();
        assert!(
            !snap.iter().any(|e| matches!(e, RecordedEvent::StreamFinalized { .. })),
            "cancel must not emit StreamFinalized; got {snap:?}"
        );
        assert!(
            snap.iter()
                .any(|e| matches!(e, RecordedEvent::System(s) if s == "(cancelled)")),
            "expected dimmed (cancelled) system note; got {snap:?}"
        );
    }

    /// P3-5: ANSI escapes inside a TokenDelta payload must be stripped
    /// before they reach the sink — otherwise a malicious LLM provider
    /// could inject CSI/OSC into the streaming frame buffer.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn tui_mirror_sanitizes_ansi_in_token_delta() {
        let sink = Arc::new(RecordingSink::default());
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let ch = TerminalChannel::new(true);
        ch.with_tui_mirror(Box::new(Arc::clone(&sink)), redraw_tx).await;

        let draft_id = ch
            .send_draft(&SendMessage::new("", "user"))
            .await
            .expect("test: send_draft ok")
            .expect("test: draft id");
        ch.update_draft("user", &draft_id, "Hello \x1b[31mred\x1b[0m world")
            .await
            .expect("test: update ok");

        assert!(
            wait_for_sink(&sink, 50, |evts| {
                evts.iter().any(|e| {
                    matches!(
                        e,
                        RecordedEvent::StreamUpdated { accumulated, .. } if accumulated == "Hello red world"
                    )
                })
            })
            .await,
            "expected sanitised StreamUpdated; got {:?}",
            sink.snapshot()
        );
    }

    /// Tool start + finish notifications under the TUI path should produce
    /// a matching pair of [`RecordedEvent::ToolStarted`] / [`ToolFinished`]
    /// translations in order.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn tui_mirror_tool_events_push_and_finish_card() {
        let sink = Arc::new(RecordingSink::default());
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let ch = TerminalChannel::new(true);
        ch.with_tui_mirror(Box::new(Arc::clone(&sink)), redraw_tx).await;

        ch.notify_tool_started("file_read", "{\"path\":\"/tmp/x\"}").await;
        ch.notify_tool_finished("file_read", true, 42).await;

        assert!(
            wait_for_sink(&sink, 50, |evts| {
                let has_started = evts
                    .iter()
                    .any(|e| matches!(e, RecordedEvent::ToolStarted { name, .. } if name == "file_read"));
                let has_finished = evts.iter().any(|e| {
                    matches!(
                        e,
                        RecordedEvent::ToolFinished { name, success: true, duration_ms: 42 }
                        if name == "file_read"
                    )
                });
                has_started && has_finished
            })
            .await,
            "expected ToolStarted+ToolFinished(file_read); got {:?}",
            sink.snapshot()
        );
    }

    /// ANSI escapes from untrusted LLM output must not survive into the
    /// sink — otherwise the renderer would faithfully repaint them into the
    /// frame buffer. Feed a CSI-laced final message; the recorded text
    /// must be stripped.
    #[cfg(feature = "terminal-tui")]
    #[tokio::test]
    async fn tui_mirror_sanitizes_ansi_in_final_message() {
        let sink = Arc::new(RecordingSink::default());
        let (redraw_tx, _redraw_rx) = mpsc::channel::<()>(1);
        let ch = TerminalChannel::new(true);
        ch.with_tui_mirror(Box::new(Arc::clone(&sink)), redraw_tx).await;

        ch.send(&SendMessage::new("Hello \x1b[31mred\x1b[0m world", "user"))
            .await
            .expect("test: send ok");

        assert!(
            wait_for_sink(&sink, 50, |evts| {
                evts.iter()
                    .any(|e| matches!(e, RecordedEvent::Assistant(c) if c == "Hello red world"))
            })
            .await,
            "expected sanitised Assistant translation; got {:?}",
            sink.snapshot()
        );
    }
}
