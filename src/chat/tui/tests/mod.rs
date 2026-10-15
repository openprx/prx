#![cfg(test)]

use super::*;

static MARKDOWN_CACHE_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Flatten a rendered `Line` into plain text for assertions.
fn line_to_plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn span_fg(lines: &[Line<'_>], line_idx: usize, span_idx: usize) -> Option<Color> {
    lines
        .get(line_idx)
        .and_then(|line| line.spans.get(span_idx))
        .and_then(|span| span.style.fg)
}

fn span_fg_for_content(lines: &[Line<'_>], content: &str) -> Option<Color> {
    lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .find(|span| span.content.as_ref() == content)
        .and_then(|span| span.style.fg)
}

// ── P2-11: DraftLineBuffer tests ─────────────────────────────────────────

fn lines(slice: &[&str]) -> Vec<String> {
    slice.iter().map(|s| (*s).to_string()).collect()
}

// ── P2-12: Reasoning card tests ──────────────────────────────────────────

/// Collapse a rendered `Line` into a plain `String` by concatenating all
/// span contents. Used so assertions can grep for substrings without
/// caring about the ratatui Span structure.
fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

// ── P2-10: TuiInput multi-line + history tests ───────────────────────────

/// Build a `KeyEvent` with no modifiers.
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Build a `KeyEvent` with a single modifier.
fn key_mod(code: KeyCode, m: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, m)
}

fn key_repeat(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Repeat,
        state: crossterm::event::KeyEventState::NONE,
    }
}

fn saved_picker_entry(id: &str, title: &str, is_current: bool) -> crate::chat::session::SavedSessionPickerEntry {
    crate::chat::session::SavedSessionPickerEntry {
        id: id.to_string(),
        title: title.to_string(),
        turn_count: 2,
        updated_at: chrono::Utc::now(),
        provider: "provider".to_string(),
        model: "model".to_string(),
        is_current,
    }
}

/// Convenience: type each char in `s` into `input`.
fn type_str(input: &mut TuiInput, s: &str) {
    for ch in s.chars() {
        input.handle_key(key(KeyCode::Char(ch)));
    }
}

fn approval_state() -> TuiState {
    let mut state = TuiState::new("p", "m");
    state.focus = crate::chat::sessions::FocusTarget::Approval;
    state.pending_tool_approval = Some(crate::chat::sessions::PendingToolApprovalView {
        task_id: None,
        tool_id: "call-1".to_string(),
        name: "shell".to_string(),
        args: r#"{"cmd":"echo hi"}"#.to_string(),
        selected_approval: false,
    });
    state
}

// ── v1.1b: switcher + Esc-detach + focus indicator ───────────────────────

fn entry(seq: u64) -> crate::chat::sessions::SwitcherEntry {
    let now = chrono::Utc::now();
    crate::chat::sessions::SwitcherEntry {
        seq,
        kind: "agent",
        origin: "user",
        status: "running",
        title: format!("task {seq}"),
        created_at: now,
        updated_at: now,
        token_usage_records: Vec::new(),
        idle_warning: false,
    }
}

fn usage_record(
    source: crate::llm::route_decision::TokenUsageSource,
) -> crate::llm::route_decision::MeteredTokenUsageRecord {
    crate::llm::route_decision::MeteredTokenUsageRecord {
        settlement_id: None,
        provider: "openai".to_string(),
        model: "gpt-4o-mini".to_string(),
        prompt_tokens: 8_000,
        completion_tokens: 4_300,
        total_tokens: 12_300,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        source,
        cost_usd: Some(0.0042),
    }
}

// ── v5: switcher row layout (origin tag + narrow-terminal degradation) ────

fn pty_entry(seq: u64, title: &str) -> crate::chat::sessions::SwitcherEntry {
    let now = chrono::Utc::now();
    crate::chat::sessions::SwitcherEntry {
        seq,
        kind: "pty",
        origin: "user",
        status: "running",
        title: title.to_string(),
        created_at: now,
        updated_at: now,
        token_usage_records: Vec::new(),
        idle_warning: false,
    }
}

fn elapsed_entry(seq: u64, status: &'static str, elapsed_seconds: i64) -> crate::chat::sessions::SwitcherEntry {
    let created_at = chrono::DateTime::parse_from_rfc3339("2026-07-04T12:00:00Z")
        .expect("test timestamp")
        .with_timezone(&chrono::Utc);
    crate::chat::sessions::SwitcherEntry {
        seq,
        kind: "agent",
        origin: "model",
        status,
        title: format!("elapsed task {seq}"),
        created_at,
        updated_at: created_at + chrono::Duration::seconds(elapsed_seconds),
        token_usage_records: Vec::new(),
        idle_warning: false,
    }
}

fn long_strip_entries(count: u64) -> Vec<crate::chat::sessions::SwitcherEntry> {
    (1..=count)
        .map(|seq| {
            let mut entry = elapsed_entry(seq, "running", i64::try_from(seq).unwrap_or(i64::MAX));
            entry.title = format!("List 3 strengths of a terminal chat UI. Return only concise bullets for run {seq}");
            entry
        })
        .collect()
}

fn active_view(seq: u64, lines: Vec<String>, scroll_offset: usize) -> crate::chat::sessions::ActiveSessionView {
    crate::chat::sessions::ActiveSessionView {
        seq,
        kind: "agent".to_string(),
        title: "\u{ac00}".repeat(13) + "-with-a-long-title",
        lines,
        truncated: true,
        scroll_offset,
    }
}

fn provider_worker_status_fixture() -> ProviderWorkerStatus {
    ProviderWorkerStatus {
        running: 1,
        cancelling: 0,
        awaiting_commit: 0,
        finalized_payloads: 0,
        finalized_total_tokens: 0,
        oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis().saturating_sub(2_000)),
        rows: vec![ProviderWorkerStatusRow {
            task_id: 42,
            sequence: 3,
            kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
            state: ProviderWorkerRowState::Running,
            started_at_ms: chrono::Utc::now().timestamp_millis().saturating_sub(2_000),
            finalized_total_tokens: None,
            completion_ready: false,
            recent_tool_call: None,
        }],
    }
}

fn fullscreen_rows(state: &TuiState, width: u16, height: u16, scroll: &mut FullscreenTranscriptScroll) -> Vec<String> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test backend");
    terminal
        .draw(|frame| {
            render_fullscreen_chat(frame, state, scroll);
        })
        .expect("draw fullscreen chat");
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect::<Vec<_>>().join(""))
        .collect()
}

fn render_conversation_line_to_buffer(buf: &mut Buffer, line: &ConversationLine, ascii: bool) {
    let mut lines: Vec<Line<'_>> = Vec::new();
    render_conversation_line(&mut lines, line, ascii);
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    paragraph.render(buf.area, buf);
}

mod cjk;
mod draft_and_reasoning;
mod input_editing;
mod key_dispatch;
mod rearch_chrome;
mod rearch_layout;
mod s4_a_2;
mod s4_a_5;
mod streaming_draft;
mod subagent_cards;
mod switcher;
mod tool_result_cards;
