//! Terminal protocol utilities: inline image preview (kitty/iTerm2),
//! OSC 52 clipboard support for code block copying, monotonic version
//! tracking for streaming draft deltas (P1-6), and the incremental
//! inline-redraw protocol for fine-grained line-range replacement (P2-11).

use async_trait::async_trait;
use base64::Engine;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

// ── Draft version protocol (P1-6) ───────────────────────────────────────────
//
// Streaming `update_draft` deltas flow through one or more mpsc channels.
// Although a single tokio mpsc preserves order, the pipeline crosses several
// channels (delta_tx → accumulator task → ui_tx → UiActor) and is interleaved
// with other tasks. A late or duplicated delta could otherwise overwrite a
// newer one and visually "rewind" the rendered text.
//
// `DraftVersionCounter` (sender side) stamps each delta with a strictly
// monotonic `u64`. `DraftVersionTracker` (receiver side) records the highest
// version seen per `draft_id` and rejects any later arrival whose version
// is not strictly greater. Version sequences are independent per draft id;
// `finalize` clears the tracked state for a draft.

/// Monotonic version generator for outgoing draft deltas.
///
/// Each call to [`next`](Self::next) returns a strictly increasing `u64`
/// starting at `1`. Safe to share across tasks via `Arc`.
#[derive(Debug, Default)]
pub struct DraftVersionCounter {
    inner: AtomicU64,
}

impl DraftVersionCounter {
    /// Create a fresh counter whose first issued version is `1`.
    pub const fn new() -> Self {
        Self {
            inner: AtomicU64::new(0),
        }
    }

    /// Allocate the next version. Versions are strictly monotonic across
    /// concurrent callers; ties are impossible.
    pub fn next(&self) -> u64 {
        self.inner.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Current highest issued version (0 if none issued yet). For diagnostics.
    pub fn current(&self) -> u64 {
        self.inner.load(Ordering::Relaxed)
    }
}

/// Receiver-side version watchdog: accepts only strictly newer versions
/// for each `draft_id`, rejecting stale or duplicate arrivals.
#[derive(Debug, Default)]
pub struct DraftVersionTracker {
    /// Map of `draft_id` → highest accepted version.
    inner: Mutex<HashMap<String, u64>>,
}

impl DraftVersionTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Attempt to accept a delta for `draft_id` at `version`.
    ///
    /// Returns `true` if `version` is strictly greater than the previously
    /// accepted version for this draft (in which case the tracker now stores
    /// `version` as the new high-water mark) or if no version has been
    /// accepted yet. Returns `false` for stale or duplicate arrivals — the
    /// caller should drop the delta.
    pub fn accept(&self, draft_id: &str, version: u64) -> bool {
        let mut guard = self.inner.lock();
        match guard.get(draft_id) {
            Some(&prev) if version <= prev => false,
            _ => {
                guard.insert(draft_id.to_string(), version);
                true
            }
        }
    }

    /// Forget version state for a draft (call on finalize/cancel).
    pub fn clear(&self, draft_id: &str) {
        self.inner.lock().remove(draft_id);
    }

    /// Current high-water version for a draft, if any.
    pub fn current(&self, draft_id: &str) -> Option<u64> {
        self.inner.lock().get(draft_id).copied()
    }

    /// Number of drafts currently tracked. For diagnostics/tests.
    pub fn tracked_count(&self) -> usize {
        self.inner.lock().len()
    }
}

// ── Incremental inline-redraw protocol (P2-11) ──────────────────────────────
//
// The base `Channel::update_draft` protocol (in `crate::channels::traits`)
// rewrites the entire accumulated draft text on every delta. This is correct
// but expensive when the only thing changing is, e.g., the last line of a
// progress bar or a single revised line in the middle of a long block.
//
// `InlineDraftProtocol` is a sibling capability trait: it lets a sender ask
// the receiver to replace a contiguous range of lines inside an existing
// draft, leaving the rest untouched. The default implementation declines
// (`LineProtocolError::NotSupported`); senders should treat this as a signal
// to fall back to `update_draft` with the full snapshot.
//
// Per draft id, line ranges are 0-indexed against the draft's current line
// vector and `line_count == 0` means "insert at `start_line` without
// removing anything". Stale or duplicate `version` values are rejected via
// `DraftVersionTracker` so that out-of-order arrivals across the streaming
// pipeline cannot visually "rewind" the rendered text.

/// Typed errors for the incremental inline-redraw protocol.
#[derive(Debug, thiserror::Error)]
pub enum LineProtocolError {
    /// The receiving channel does not implement fine-grained line redraw.
    /// Callers should fall back to a full `update_draft` snapshot.
    #[error("incremental line redraw not supported by this channel")]
    NotSupported,

    /// The arriving `version` is not strictly greater than the last accepted
    /// version for this draft. The delta has been dropped.
    #[error("stale draft version {got} (current high-water mark: {current})")]
    StaleVersion { got: u64, current: u64 },

    /// The requested `[start_line, start_line + line_count)` range exceeds the
    /// current draft length. The buffer was not mutated.
    #[error("line range out of bounds: start={start}, count={count}, current_len={current_len}")]
    RangeOutOfBounds {
        start: usize,
        count: usize,
        current_len: usize,
    },

    /// No draft with the given id is currently tracked.
    #[error("unknown draft id `{0}`")]
    UnknownDraft(String),
}

/// Apply a line-range replacement to an in-place `Vec<String>` buffer.
///
/// - `start` is the 0-indexed line at which the replacement begins.
/// - `count` is the number of existing lines to remove. `count == 0` makes
///   this an insertion at `start`.
/// - `new_content` is split on `'\n'` and each segment becomes one line.
///   An empty `new_content` inserts nothing.
///
/// Returns `RangeOutOfBounds` if `start + count` exceeds `lines.len()`.
/// On error the buffer is left unmodified.
pub fn apply_line_replacement(
    lines: &mut Vec<String>,
    start: usize,
    count: usize,
    new_content: &str,
) -> Result<(), LineProtocolError> {
    let end = start.checked_add(count).ok_or(LineProtocolError::RangeOutOfBounds {
        start,
        count,
        current_len: lines.len(),
    })?;
    if end > lines.len() {
        return Err(LineProtocolError::RangeOutOfBounds {
            start,
            count,
            current_len: lines.len(),
        });
    }
    // `new_content == ""` should insert zero lines (not a single empty line),
    // matching the natural "delete N lines" semantics when count > 0.
    let replacement: Vec<String> = if new_content.is_empty() {
        Vec::new()
    } else {
        new_content.split('\n').map(str::to_owned).collect()
    };
    lines.splice(start..end, replacement);
    Ok(())
}

/// Fine-grained inline-redraw capability: replace `[start_line, start_line + line_count)`
/// inside an existing draft without rewriting the whole snapshot.
///
/// This is intentionally a **sibling** trait of `crate::channels::traits::Channel`
/// rather than an extension of it. Implementations that don't need precise
/// redraws simply don't implement this trait, and senders can detect the
/// `LineProtocolError::NotSupported` default and fall back to `update_draft`.
#[async_trait]
pub trait InlineDraftProtocol: Send + Sync {
    /// Replace lines `[start_line, start_line + line_count)` of `draft_id`
    /// with `new_content` (split on `'\n'`).
    ///
    /// - `role` is the conversation role (e.g. `"assistant"`); implementations
    ///   that don't distinguish may ignore it.
    /// - `version` must come from a [`DraftVersionCounter`] shared with all
    ///   producers writing into this draft. Stale arrivals are rejected.
    ///
    /// The default implementation rejects all calls with
    /// [`LineProtocolError::NotSupported`].
    async fn replace_lines(
        &self,
        _role: &str,
        _draft_id: &str,
        _start_line: usize,
        _line_count: usize,
        _new_content: &str,
        _version: u64,
    ) -> Result<(), LineProtocolError> {
        Err(LineProtocolError::NotSupported)
    }
}

/// Detect if the terminal supports kitty graphics protocol.
pub fn supports_kitty_graphics() -> bool {
    std::env::var("TERM_PROGRAM").map(|v| v == "kitty").unwrap_or(false) || std::env::var("KITTY_PID").is_ok()
}

/// Detect if the terminal supports iTerm2 inline image protocol.
pub fn supports_iterm2_images() -> bool {
    std::env::var("TERM_PROGRAM")
        .map(|v| v == "iTerm.app" || v == "WezTerm")
        .unwrap_or(false)
        || std::env::var("ITERM_SESSION_ID").is_ok()
}

/// Display an image inline using the appropriate terminal protocol.
///
/// Falls back to printing a text description if no image protocol is supported.
pub fn display_image(path: &str) -> io::Result<()> {
    let data = std::fs::read(path)?;

    if supports_kitty_graphics() {
        display_image_kitty(&data)
    } else if supports_iterm2_images() {
        display_image_iterm2(&data, path)
    } else {
        println!("  [image: {path}]");
        Ok(())
    }
}

/// Display image using kitty graphics protocol.
fn display_image_kitty(data: &[u8]) -> io::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let mut stdout = io::stdout().lock();

    // Kitty protocol: split into 4096-byte chunks
    let chunk_size = 4096;
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(chunk_size)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let more = if i < chunks.len() - 1 { 1 } else { 0 };
        if i == 0 {
            write!(stdout, "\x1b_Ga=T,f=100,m={more};{chunk}\x1b\\")?;
        } else {
            write!(stdout, "\x1b_Gm={more};{chunk}\x1b\\")?;
        }
    }
    writeln!(stdout)?;
    stdout.flush()
}

/// Display image using iTerm2 inline image protocol.
fn display_image_iterm2(data: &[u8], name: &str) -> io::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let filename_b64 = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
    let mut stdout = io::stdout().lock();
    write!(
        stdout,
        "\x1b]1337;File=name={filename_b64};size={};inline=1:{b64}\x07",
        data.len()
    )?;
    writeln!(stdout)?;
    stdout.flush()
}

/// Copy text to clipboard using OSC 52 escape sequence.
///
/// Works in terminals that support OSC 52 (xterm, kitty, iTerm2, WezTerm, etc.).
pub fn copy_to_clipboard(text: &str) -> io::Result<()> {
    if let Err(error) = copy_to_tmux_buffer(text) {
        tracing::debug!(%error, "tmux clipboard handoff failed; falling back to OSC 52");
    }
    let mut stdout = io::stdout().lock();
    write!(stdout, "{}", osc52_clipboard_sequence(text))?;
    stdout.flush()
}

fn osc52_clipboard_sequence(text: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    // OSC 52: set clipboard content; "c" means CLIPBOARD selection.
    format!("\x1b]52;c;{b64}\x07")
}

fn copy_to_tmux_buffer(text: &str) -> io::Result<()> {
    if std::env::var_os("TMUX").is_none() {
        return Ok(());
    }

    let mut child = Command::new("tmux")
        .args(["load-buffer", "-w", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("tmux load-buffer -w failed"))
    }
}

/// Chat theme configuration.
#[derive(Debug, Clone)]
pub struct ChatTheme {
    pub user_color: &'static str,
    pub assistant_color: &'static str,
    pub tool_color: &'static str,
    pub error_color: &'static str,
    pub status_color: &'static str,
    pub muted_color: &'static str,
}

impl ChatTheme {
    /// Dark theme (default).
    pub const fn dark() -> Self {
        Self {
            user_color: "\x1b[32m",      // green
            assistant_color: "\x1b[36m", // cyan
            tool_color: "\x1b[33m",      // yellow
            error_color: "\x1b[31m",     // red
            status_color: "\x1b[37m",    // white
            muted_color: "\x1b[90m",     // dark gray
        }
    }

    /// Light theme.
    pub const fn light() -> Self {
        Self {
            user_color: "\x1b[34m",      // blue
            assistant_color: "\x1b[35m", // magenta
            tool_color: "\x1b[33m",      // yellow
            error_color: "\x1b[31m",     // red
            status_color: "\x1b[30m",    // black
            muted_color: "\x1b[37m",     // light gray
        }
    }

    /// Monokai-inspired theme.
    pub const fn monokai() -> Self {
        Self {
            user_color: "\x1b[38;2;166;226;46m",       // monokai green
            assistant_color: "\x1b[38;2;102;217;239m", // monokai cyan
            tool_color: "\x1b[38;2;253;151;31m",       // monokai orange
            error_color: "\x1b[38;2;249;38;114m",      // monokai pink
            status_color: "\x1b[38;2;248;248;242m",    // monokai fg
            muted_color: "\x1b[38;2;117;113;94m",      // monokai comment
        }
    }

    /// ANSI reset sequence.
    pub const fn reset() -> &'static str {
        "\x1b[0m"
    }

    /// Get theme by name.
    pub fn by_name(name: &str) -> Self {
        match name {
            "light" => Self::light(),
            "monokai" => Self::monokai(),
            _ => Self::dark(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_dark_default() {
        let theme = ChatTheme::dark();
        assert!(theme.user_color.contains("\x1b["));
        assert!(!ChatTheme::reset().is_empty());
    }

    #[test]
    fn theme_by_name() {
        let dark = ChatTheme::by_name("dark");
        assert!(dark.user_color.contains("32m"));
        let light = ChatTheme::by_name("light");
        assert!(light.user_color.contains("34m"));
        let mono = ChatTheme::by_name("monokai");
        assert!(mono.user_color.contains("38;2;"));
    }

    #[test]
    fn osc52_clipboard_format() {
        assert_eq!(
            osc52_clipboard_sequence("hello world"),
            "\x1b]52;c;aGVsbG8gd29ybGQ=\x07"
        );
    }

    #[test]
    fn kitty_detection() {
        // In test env, likely false
        let result = supports_kitty_graphics();
        assert!(!result || result); // just verify it doesn't panic
    }

    #[test]
    fn iterm2_detection() {
        let result = supports_iterm2_images();
        assert!(!result || result);
    }

    // ── Draft version protocol tests (P1-6) ─────────────────────────────────

    #[test]
    fn draft_version_counter_is_monotonic() {
        let counter = DraftVersionCounter::new();
        assert_eq!(counter.current(), 0);
        assert_eq!(counter.next(), 1);
        assert_eq!(counter.next(), 2);
        assert_eq!(counter.next(), 3);
        assert_eq!(counter.current(), 3);
    }

    #[test]
    fn draft_version_counter_concurrent_unique() {
        use std::sync::Arc;
        use std::thread;

        let counter = Arc::new(DraftVersionCounter::new());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = Arc::clone(&counter);
            handles.push(thread::spawn(move || (0..100).map(|_| c.next()).collect::<Vec<u64>>()));
        }
        let mut all: Vec<u64> = handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect();
        all.sort_unstable();
        let dedup_len = {
            let mut v = all.clone();
            v.dedup();
            v.len()
        };
        assert_eq!(all.len(), 800);
        assert_eq!(dedup_len, 800, "versions must be unique across threads");
        assert_eq!(counter.current(), 800);
    }

    #[test]
    fn tracker_accepts_sequential_versions() {
        let tracker = DraftVersionTracker::new();
        assert!(tracker.accept("draft-a", 1));
        assert!(tracker.accept("draft-a", 2));
        assert!(tracker.accept("draft-a", 3));
        assert_eq!(tracker.current("draft-a"), Some(3));
    }

    #[test]
    fn tracker_rejects_stale_versions() {
        let tracker = DraftVersionTracker::new();
        assert!(tracker.accept("draft-a", 3));
        // Out-of-order older arrival → drop
        assert!(!tracker.accept("draft-a", 1));
        // Exact duplicate → drop
        assert!(!tracker.accept("draft-a", 3));
        // Newer still accepted
        assert!(tracker.accept("draft-a", 4));
        assert_eq!(tracker.current("draft-a"), Some(4));
    }

    #[test]
    fn tracker_per_draft_id_independent() {
        let tracker = DraftVersionTracker::new();
        assert!(tracker.accept("draft-a", 5));
        // A new draft id starts from scratch — version 1 is fresh.
        assert!(tracker.accept("draft-b", 1));
        assert!(tracker.accept("draft-b", 2));
        // draft-a still rejects anything ≤ 5
        assert!(!tracker.accept("draft-a", 5));
        assert!(tracker.accept("draft-a", 6));
        assert_eq!(tracker.current("draft-a"), Some(6));
        assert_eq!(tracker.current("draft-b"), Some(2));
        assert_eq!(tracker.tracked_count(), 2);
    }

    #[test]
    fn tracker_clear_releases_state() {
        let tracker = DraftVersionTracker::new();
        assert!(tracker.accept("draft-a", 10));
        assert_eq!(tracker.tracked_count(), 1);
        tracker.clear("draft-a");
        assert_eq!(tracker.tracked_count(), 0);
        assert_eq!(tracker.current("draft-a"), None);
        // After clear, low versions are fresh again (new draft lifecycle).
        assert!(tracker.accept("draft-a", 1));
        assert_eq!(tracker.current("draft-a"), Some(1));
    }

    #[test]
    fn tracker_out_of_order_keeps_newest() {
        // Simulate cross-channel reordering: v1 → v3 → v2 → v4
        let tracker = DraftVersionTracker::new();
        let counter = DraftVersionCounter::new();
        let v1 = counter.next();
        let v2 = counter.next();
        let v3 = counter.next();
        let v4 = counter.next();

        // Arrives out of order: v3 first, then stale v1, v2, then v4.
        assert!(tracker.accept("d", v3));
        assert!(!tracker.accept("d", v1), "v1 < v3 must be dropped");
        assert!(!tracker.accept("d", v2), "v2 < v3 must be dropped");
        assert!(tracker.accept("d", v4), "v4 > v3 must be accepted");
        assert_eq!(tracker.current("d"), Some(v4));
    }

    #[test]
    fn counter_and_tracker_compose_for_in_order_stream() {
        let counter = DraftVersionCounter::new();
        let tracker = DraftVersionTracker::new();
        for expected in 1..=10u64 {
            let v = counter.next();
            assert_eq!(v, expected);
            assert!(tracker.accept("draft-x", v));
        }
        assert_eq!(tracker.current("draft-x"), Some(10));
    }

    // ── Incremental inline-redraw protocol tests (P2-11) ────────────────────

    fn lines_of(slice: &[&str]) -> Vec<String> {
        slice.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn apply_line_replacement_replaces_middle_range() {
        let mut buf = lines_of(&["a", "b", "c", "d", "e"]);
        apply_line_replacement(&mut buf, 1, 3, "X\nY").expect("test: in-range replace");
        assert_eq!(buf, lines_of(&["a", "X", "Y", "e"]));
    }

    #[test]
    fn apply_line_replacement_with_zero_count_inserts() {
        let mut buf = lines_of(&["a", "b", "c"]);
        apply_line_replacement(&mut buf, 2, 0, "X\nY").expect("test: insert");
        assert_eq!(buf, lines_of(&["a", "b", "X", "Y", "c"]));
    }

    #[test]
    fn apply_line_replacement_empty_new_content_deletes_range() {
        let mut buf = lines_of(&["a", "b", "c", "d"]);
        apply_line_replacement(&mut buf, 1, 2, "").expect("test: delete-only");
        assert_eq!(buf, lines_of(&["a", "d"]));
    }

    #[test]
    fn apply_line_replacement_rejects_out_of_bounds() {
        let mut buf = lines_of(&["a", "b"]);
        let err = apply_line_replacement(&mut buf, 1, 5, "X").expect_err("test: must reject");
        assert!(matches!(
            err,
            LineProtocolError::RangeOutOfBounds {
                start: 1,
                count: 5,
                current_len: 2
            }
        ));
        // Buffer must be untouched on error.
        assert_eq!(buf, lines_of(&["a", "b"]));
    }

    #[test]
    fn apply_line_replacement_rejects_overflow() {
        let mut buf = lines_of(&["a"]);
        let err = apply_line_replacement(&mut buf, usize::MAX, 1, "X").expect_err("test: overflow");
        assert!(matches!(err, LineProtocolError::RangeOutOfBounds { .. }));
        assert_eq!(buf, lines_of(&["a"]));
    }

    #[test]
    fn apply_line_replacement_at_end_is_append() {
        let mut buf = lines_of(&["a", "b"]);
        apply_line_replacement(&mut buf, 2, 0, "c\nd").expect("test: append");
        assert_eq!(buf, lines_of(&["a", "b", "c", "d"]));
    }

    /// Fixture exercising only the trait default impl (no override). Verifies
    /// the not-supported fallback contract for channels that do not implement
    /// fine-grained line redraw.
    struct DefaultOnlyChannel;
    #[async_trait]
    impl InlineDraftProtocol for DefaultOnlyChannel {}

    #[tokio::test]
    async fn default_impl_returns_not_supported() {
        let ch = DefaultOnlyChannel;
        let err = ch
            .replace_lines("assistant", "draft-1", 0, 1, "X", 1)
            .await
            .expect_err("test: default must decline");
        assert!(matches!(err, LineProtocolError::NotSupported));
    }
}
