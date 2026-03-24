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
use std::io::{self, Write as _};
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
        }
    }

    /// Main event loop — runs until Shutdown event or channel close.
    async fn run(mut self) {
        while let Some(event) = self.event_rx.recv().await {
            match event {
                UiEvent::Shutdown => break,
                other => self.handle_event(other),
            }
        }
    }

    fn handle_event(&mut self, event: UiEvent) {
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
                    print!("{safe_content}");
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
                        println!("\n{safe}\n");
                    }
                    return;
                }

                self.state = UiState::Finalizing;

                // Flush any remaining un-rendered delta (safe char-boundary + sanitize)
                if full_text.len() > self.draft_buffer.len() && full_text.is_char_boundary(self.draft_buffer.len()) {
                    let remaining = sanitize_terminal_output(&full_text[self.draft_buffer.len()..]);
                    print!("{remaining}");
                }
                println!();
                println!();
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
                        println!();
                        if !self.plain_mode {
                            println!("  (cancelled)");
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
                    println!("\n{safe}\n");
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

/// Interactive input loop using reedline.
///
/// Reads user input, recognizes slash-commands, and sends `ChannelMessage`s
/// to the agent pipeline.
async fn terminal_input_loop(tx: mpsc::Sender<ChannelMessage>) -> Result<()> {
    use reedline::{DefaultPrompt, DefaultPromptSegment, FileBackedHistory, Reedline, Signal};

    // Run reedline on a blocking thread since it blocks on stdin
    let handle = tokio::task::spawn_blocking(move || -> Result<()> {
        // Set up file-backed input history
        let history_path = directories::ProjectDirs::from("dev", "openprx", "prx")
            .map(|dirs| dirs.data_dir().join("chat_history"))
            .unwrap_or_else(|| std::path::PathBuf::from(".prx_chat_history"));
        let mut editor = FileBackedHistory::with_file(1000, history_path).map_or_else(
            |_| Reedline::create(),
            |history| Reedline::create().with_history(Box::new(history)),
        );
        let prompt = DefaultPrompt::new(
            DefaultPromptSegment::Basic("prx".to_string()),
            DefaultPromptSegment::Empty,
        );

        loop {
            match editor.read_line(&prompt) {
                Ok(Signal::Success(line)) => {
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

                    // Use blocking send via a new runtime-less channel
                    if tx.blocking_send(msg).is_err() {
                        break;
                    }
                }
                Ok(Signal::CtrlC) => {
                    // Cancel current input, continue
                    continue;
                }
                Ok(Signal::CtrlD) => {
                    // Exit
                    break;
                }
                Err(_) => break,
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
}
