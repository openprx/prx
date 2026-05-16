//! `prx chat` entry point — rich terminal interactive chat.
//!
//! Wires up the full agent pipeline (memory, tools, providers, security, hooks,
//! observability) and uses [`TerminalChannel`] for streaming I/O through the
//! event-driven UI Actor.
// Chat module: println!/eprintln! are intentional user-facing output (banners, status, errors).
#![allow(clippy::print_stdout, clippy::print_stderr)]

pub mod action;
pub mod commands;
pub mod dispatcher;
pub mod error;
pub mod sanitize;
pub mod session;
pub mod state;
pub mod terminal_proto;

#[cfg(feature = "terminal-tui")]
pub mod renderer;
#[cfg(feature = "terminal-tui")]
pub mod tui;

use crate::agent::loop_::{
    ScopeContext, ToolCallNotification, ToolConcurrencyGovernanceConfig, build_context, build_runtime_system_prompt,
    increment_recalled_useful_counts, is_tool_loop_cancelled, run_tool_call_loop, select_prompt_skills,
};
use crate::approval::ApprovalManager;
use crate::channels::traits::extract_outgoing_media;
use crate::channels::{
    Channel, SendMessage, TerminalChannel, extract_tool_context_summary, is_context_window_overflow_error,
    sanitize_channel_response,
};
use crate::chat::terminal_proto::DraftVersionCounter;
use crate::config::Config;
use crate::hooks::{HookEvent, HookManager, payload_error};
use crate::memory::{self, Memory, MemoryCategory};
use crate::observability::{self, Observer, ObserverEvent};
use crate::providers::{self, ChatMessage, Provider};
use crate::runtime;
use crate::security::PolicyPipeline;
use crate::security::SecurityPolicy;
use crate::tools::{self, Tool};
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Minimum user-message length for auto-save to memory.
const AUTOSAVE_MIN_MESSAGE_CHARS: usize = 10;

/// Window (ms) for double Ctrl+C to trigger exit.
const DOUBLE_CTRLC_WINDOW_MS: u64 = 500;

/// Max retries on context window overflow (compact history + retry).
const MAX_CONTEXT_OVERFLOW_RETRIES: usize = 2;

/// Keep last N non-system messages during history compaction.
const COMPACT_KEEP_MESSAGES: usize = 8;

/// Per-message character limit during compaction.
const COMPACT_CONTENT_CHARS: usize = 320;

/// Total character budget for compacted history (excluding system prompt).
const COMPACT_TOTAL_CHARS: usize = 2400;

/// Capacity for the user-input mpsc channel.
const INPUT_CHANNEL_CAPACITY: usize = 16;

/// Capacity for the streaming delta (partial response) mpsc channel.
const DELTA_CHANNEL_CAPACITY: usize = 64;

/// Capacity for the tool-call notification mpsc channel (visual feedback).
const TOOL_EVENT_CHANNEL_CAPACITY: usize = 32;

/// Minimum base timeout (seconds) for per-turn timeout budget.
const TIMEOUT_MIN_BASE_SECS: u64 = 30;

/// Maximum multiplier applied to the base timeout (caps iterations-based scaling).
const TIMEOUT_MAX_SCALE_FACTOR: u64 = 4;

/// Compact conversation history in-place to fit within context window limits.
///
/// Preserves the system prompt (index 0), keeps the last [`COMPACT_KEEP_MESSAGES`]
/// non-system messages, truncates each to [`COMPACT_CONTENT_CHARS`], and enforces
/// a total character budget of [`COMPACT_TOTAL_CHARS`].
fn compact_chat_history(history: &mut Vec<ChatMessage>) {
    if history.len() <= 1 {
        return;
    }

    // Separate system prompt from conversation turns
    let has_system = history.first().map(|m| m.role == "system").unwrap_or(false);
    let start = if has_system { 1 } else { 0 };

    // Keep only the last COMPACT_KEEP_MESSAGES conversation turns
    let turn_count = history.len() - start;
    if turn_count > COMPACT_KEEP_MESSAGES {
        let drain_end = start + turn_count - COMPACT_KEEP_MESSAGES;
        history.drain(start..drain_end);
    }

    // Truncate individual messages
    for msg in history.iter_mut().skip(start) {
        if msg.content.chars().count() > COMPACT_CONTENT_CHARS {
            msg.content = truncate_with_ellipsis(&msg.content, COMPACT_CONTENT_CHARS);
        }
    }

    // Enforce total character budget (drop oldest turns first)
    while history
        .iter()
        .skip(start)
        .map(|m| m.content.chars().count())
        .sum::<usize>()
        > COMPACT_TOTAL_CHARS
        && history.len() > start + 1
    {
        history.remove(start);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Chat 输入路径的 Redux 灰度模式. 由环境变量 `PRX_CHAT_REDUX` 控制.
///
/// - `Off`:  旧路径单写（默认）。reducer 不构造、不执行。
/// - `Both`: 双写。Event 同时分发到旧路径和 reducer，两者并行 mutate；
///   用于开发/测试期对账 reducer 与旧实现的行为一致。
/// - `Redux`: 新路径主导关键控制流（Quit / forward-delete 等），保留作为
///   `Both` 的别名以维持向后兼容（早期用 `PRX_CHAT_REDUX=1` 启用）。
/// - `Pure`: S3 T3-3 收官模式，reducer 单路由。driver 路径**默认开启**（无需
///   `PRX_CHAT_REDUX_DRIVER=1`），legacy 守卫全关，`chat_session.add_*_turn`
///   不再执行（由 reducer 的 `RecordUserTurn`/`RecordAssistantTurn` +
///   `Effect::SaveSession` 接管）。
// S2.5 T2.5-4 (T3-3-fixB 后): PRX_CHAT_REDUX 三态环境变量保留至 S5 验收完成。
//
// 当前四态实际语义（与字面"三态"区分）:
// - Off:   reducer 完全静默，旧路径单写（legacy chat_session 是唯一持久化源）
// - Both:  reducer 并行，dual_write_guard 守卫旧路径（双写防护期间）
// - Redux: 同 Both（别名，历史兼容）
// - Pure:  reducer 单路由，旧路径全关闭（T3-3 收官目标态）
//
// 移除节奏：S4-B 仅删 chat_mirror/active_cancel 等数据结构，env 守卫保留；
// S5 验收全 PASS 后再删 enum + 全部守卫调用点。
#[cfg(feature = "terminal-tui")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReduxMode {
    Off,
    Both,
    Redux,
    Pure,
}

#[cfg(feature = "terminal-tui")]
impl ReduxMode {
    /// 解析环境变量 `PRX_CHAT_REDUX`. 默认 `Off`.
    ///
    /// 值大小写不敏感，识别规则：
    /// - `""` / `"0"` / `"off"` / `"legacy"` / 未设置 → [`Self::Off`]
    /// - `"1"` / `"both"` → [`Self::Both`]（向后兼容：原 `Redux` 别名也归此）
    /// - `"redux"` → [`Self::Redux`]（显式别名，等价 `Both` + driver opt-in 语义）
    /// - `"pure"` / `"2"` → [`Self::Pure`]（T3-3 收官模式，reducer 单路由）
    /// - 其他未识别值 → [`Self::Off`]（fail-safe，避免误升级）
    fn from_env() -> Self {
        std::env::var("PRX_CHAT_REDUX")
            .ok()
            .map_or(Self::Off, |raw| Self::parse(&raw))
    }

    /// 解析单个字符串值（大小写不敏感）；fail-safe 到 `Off`.
    ///
    /// 拆成独立 fn 让 env 解析逻辑可单测，不依赖进程级 env 状态.
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "off" | "legacy" => Self::Off,
            "1" | "both" => Self::Both,
            "redux" => Self::Redux,
            "pure" | "2" => Self::Pure,
            _ => Self::Off,
        }
    }

    /// 是否启用 reducer 路径（构造 + 执行 Effect）.
    ///
    /// `Off` → false（reducer 完全静默，旧路径单写）.
    /// `Both` / `Redux` / `Pure` → true.
    #[must_use]
    pub(crate) const fn reducer_active(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// 是否在 Pure 模式（reducer 单路由，legacy 守卫全关）.
    ///
    /// T3-3-c 关键判断：`Pure` 时跳过 `chat_session.add_*_turn` 并让 reducer 的
    /// `Effect::SaveSession` 单写持久化；`Off` / `Both` / `Redux` 保留 legacy.
    #[must_use]
    pub(crate) const fn is_pure(self) -> bool {
        matches!(self, Self::Pure)
    }
}

/// Step 5a-4: chat::run 主循环 LLM turn 路由结果.
///
/// "切闸"决策由两个独立环境变量正交控制：
/// - `PRX_CHAT_REDUX=1` → reducer 模式（[`ReduxMode::Redux`]）— driver 切闸的前置条件
/// - `PRX_CHAT_REDUX_DRIVER=1` → 在 Redux 模式下显式 opt-in dispatcher driver
///
/// **5a-6 更新**：driver 现已支持 tool turn ( `drive_start_turn_stream` 收到
/// `ToolCallChunk` 后通过 `EffectDeps::tools_registry` 执行 + 多轮回合).
/// 命中 [`TurnRoute::ReduxDriver`] 仅需 `PRX_CHAT_REDUX=1` + `PRX_CHAT_REDUX_DRIVER=1`，
/// `tools_registry` 是否为空不再是路由条件。但 driver 未覆盖 approval / multimodal /
/// parallel / compaction / tiering 等高级场景，这些需要时仍走 legacy `run_tool_call_loop`
/// (后续 step 渐进迁移).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Off / TUI 关闭场景下未必引用所有 variant
pub(crate) enum TurnRoute {
    /// 走旧 `run_tool_call_loop`（生产默认）.
    LegacyToolLoop,
    /// 走 Redux dispatcher driver（仅当 Redux + driver opt-in + 无 tools）.
    ReduxDriver,
}

/// 解析 `PRX_CHAT_REDUX_DRIVER` env 是否启用 driver 切闸.
#[cfg(feature = "terminal-tui")]
#[allow(dead_code)]
fn driver_opt_in_from_env() -> bool {
    matches!(std::env::var("PRX_CHAT_REDUX_DRIVER").as_deref(), Ok("1"))
}

/// 测试钩子：`PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS=1` 强制把 tools_registry 视为空，
/// 让 5a-4 driver 切闸路径在 PTY E2E 中能被命中验证（生产环境核心工具硬编码非空）.
///
/// 仅影响路由判定 — tools_registry 内容本身**不**被清空，所以 legacy 路径分支
/// 不受影响。命名带 `FORCE_EMPTY_TOOLS` 让生产部署 grep 容易识别为测试旁路。
///
/// **5a-5 Codex P0 修复**：用 `cfg(any(test, feature = "test-mock"))` 编译期门控
/// — release build 不含 `test-mock` feature 时该函数永远返回 `false`，env 完全无效，
/// 杜绝生产环境误开旁路丢失工具能力的风险。运行时 warn 只是兜底，不是终极防线。
#[cfg(feature = "terminal-tui")]
#[allow(dead_code)]
#[allow(clippy::missing_const_for_fn)]
fn force_empty_tools_from_env() -> bool {
    // 编译期门控：只有 test build 或显式 test-mock feature 才允许读取这个 env.
    // 标准 release build (cargo build --release --features terminal-tui) 直接走
    // 下方 `false` 兜底分支，与 env 值无关 — 即便 attacker 设了该 env 也无效.
    #[cfg(any(test, feature = "test-mock"))]
    {
        matches!(
            std::env::var("PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS").as_deref(),
            Ok("1")
        )
    }
    #[cfg(not(any(test, feature = "test-mock")))]
    {
        false
    }
}

/// Step 5a-4 路由契约函数：根据 ReduxMode + driver opt-in 决定本轮 turn 走哪条路径.
///
/// **5a-6 更新**：`tools_empty` 限制已移除. driver 现支持 tool turn (`drive_start_turn_stream`
/// 收到 `ToolCallChunk` 时通过 `tools_registry` 执行 + 多轮回合). 保留 `driver_opt_in`
/// env 闸 (`PRX_CHAT_REDUX_DRIVER`) 作为生产灰度开关; 默认仍走 legacy.
///
/// 路由真值表 (TUI feature):
/// | mode  | driver_opt_in | 路由           |
/// |-------|---------------|----------------|
/// | Off   | *             | LegacyToolLoop |
/// | Both  | *             | LegacyToolLoop |
/// | Redux | false         | LegacyToolLoop |
/// | Redux | true          | ReduxDriver    |
/// | Pure  | *             | ReduxDriver    |
///
/// `Pure` 模式下 `driver_opt_in` 不再需要——T3-3 收官把 reducer 路由设为默认。
///
/// 非 TUI feature 下永远返回 [`TurnRoute::LegacyToolLoop`].
///
/// 注意 driver **未覆盖**的 legacy 能力（5a-6 故意保守）:
/// - approval_manager (危险 tool 审批)
/// - multimodal 图片校验
/// - parallel tools / scope_ctx / 并发治理
/// - context overflow 自动 compaction
/// - tool tiering / priority scheduling
///
/// 这些场景上游 (run_tool_call_loop) 仍主导, 由后续 step 渐进迁移.
#[cfg(feature = "terminal-tui")]
#[must_use]
pub(crate) const fn route_turn(mode: ReduxMode, driver_opt_in: bool, _tools_empty: bool) -> TurnRoute {
    match mode {
        // T3-3 收官：Pure 必走 driver，不再依赖 driver_opt_in env.
        ReduxMode::Pure => TurnRoute::ReduxDriver,
        // 向后兼容：Redux 需 driver opt-in 才切换；保持 5a-4 的"显式 opt-in"语义.
        ReduxMode::Redux if driver_opt_in => TurnRoute::ReduxDriver,
        _ => TurnRoute::LegacyToolLoop,
    }
}

#[cfg(not(feature = "terminal-tui"))]
#[must_use]
#[allow(dead_code)]
pub(crate) const fn route_turn(_mode: (), _driver_opt_in: bool, _tools_empty: bool) -> TurnRoute {
    TurnRoute::LegacyToolLoop
}

/// P1-2: Both 模式下的累计差异计数器.
///
/// 每次 `log_redux_key_diff` 检测到旧路径 dispatch 与新路径 Effect 存在语义差异时 += 1.
/// 测试可通过 [`redux_diff_count`] 查询该值，断言双写期行为一致（期望 0）。
#[cfg(feature = "terminal-tui")]
static REDUX_DIFF_COUNT: AtomicU64 = AtomicU64::new(0);

/// 查询 Both 模式下累计的对账差异次数（供测试断言用）.
#[cfg(feature = "terminal-tui")]
#[allow(dead_code)]
pub fn redux_diff_count() -> u64 {
    REDUX_DIFF_COUNT.load(Ordering::Relaxed)
}

/// 重置对账差异计数器（测试间隔离用）.
#[cfg(feature = "terminal-tui")]
#[allow(dead_code)]
pub fn reset_redux_diff_count() {
    REDUX_DIFF_COUNT.store(0, Ordering::Relaxed);
}

/// Both 模式下记录旧路径 dispatch 与 reducer Effect 列表的差异（tracing::debug + 计数器）.
///
/// 用于 Step 2 双写期对账：若关键控制流（Quit / Submit）在两侧产生不同输出，
/// 该日志能在 PTY 测试日志里高亮出来，同时 `REDUX_DIFF_COUNT` += 1 供测试断言。
/// Step 5 删除旧路径后移除。
///
/// P2-5: 补充字段级比对——检测 Quit 语义差异（旧路径 Exit vs 新路径 Quit）和
/// Submitted 语义差异（旧路径 Submitted vs 新路径含 LogTrace）。
#[cfg(feature = "terminal-tui")]
fn log_redux_key_diff(old: &tui::KeyDispatch, new_effects: &[state::Effect]) {
    use state::Effect;
    let old_kind = match old {
        tui::KeyDispatch::Submitted(_) => "Submitted",
        tui::KeyDispatch::Exit => "Exit",
        tui::KeyDispatch::InterruptTurn => "InterruptTurn",
        tui::KeyDispatch::Cancelled => "Cancelled",
        tui::KeyDispatch::Consumed => "Consumed",
    };
    let new_kinds: Vec<&'static str> = new_effects
        .iter()
        .map(|e| match e {
            Effect::Quit => "Quit",
            Effect::RequestRedraw => "RequestRedraw",
            Effect::LogTrace { .. } => "LogTrace",
            Effect::StartTurn { .. } => "StartTurn",
            Effect::SaveSession(_) => "SaveSession",
            Effect::SendDraftFinalize { .. } => "SendDraftFinalize",
            Effect::CancelDraft(_) => "CancelDraft",
            Effect::CancelToken(_) => "CancelToken",
            Effect::EmitChannelMessage(_) => "EmitChannelMessage",
            Effect::PersistToMemory { .. } => "PersistToMemory",
            Effect::NotifyHook { .. } => "NotifyHook",
            Effect::DisplayMedia { .. } => "DisplayMedia",
            Effect::AutoTitleSession(_) => "AutoTitleSession",
            Effect::RequestApproval { .. } => "RequestApproval",
        })
        .collect();

    // P1-2 + P2-5: 字段级语义差异检测——比对关键控制流分类是否一致.
    // 差异定义：
    //   1. 旧路径 Exit（Ctrl+D 空 buffer）≠ 新路径无 Quit
    //   2. 旧路径无 Exit，但新路径有 Quit（reducer 检测到双 Ctrl+C 或 Ctrl+D）
    //   3. 旧路径 Submitted，但新路径无 LogTrace（InputSubmitted 路径未触发）
    let new_has_quit = new_effects.iter().any(|e| matches!(e, Effect::Quit));
    let new_has_log_trace = new_effects.iter().any(|e| matches!(e, Effect::LogTrace { .. }));

    let is_diff = match old {
        tui::KeyDispatch::Exit => !new_has_quit,
        tui::KeyDispatch::Submitted(_) => !new_has_log_trace,
        // Ctrl+C → InterruptTurn in old path; new path either returns [] or Quit.
        // 只在新路径意外产生 Quit（但旧路径没有 Exit 语义）时记为差异.
        tui::KeyDispatch::InterruptTurn | tui::KeyDispatch::Cancelled | tui::KeyDispatch::Consumed => new_has_quit,
    };

    if is_diff {
        REDUX_DIFF_COUNT.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            old_dispatch = old_kind,
            new_effects = ?new_kinds,
            diff_count = REDUX_DIFF_COUNT.load(Ordering::Relaxed),
            "redux:both SEMANTIC DIFF detected"
        );
    } else {
        tracing::debug!(
            old_dispatch = old_kind,
            new_effects = ?new_kinds,
            "redux:both ok"
        );
    }
}

fn autosave_memory_key(prefix: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{prefix}:{ts}")
}

/// Aggregate the model's reasoning/thinking content from the turn's history
/// slice. The agent loop encodes assistant turns that carried reasoning as a
/// JSON object containing `{"reasoning_content": "..."}` (see
/// `build_native_assistant_history` in `agent/loop_.rs`). We pull those out
/// and join them with blank-line separators so the TUI can render a single
/// foldable card per turn.
///
/// Returns an empty string when no reasoning is present, signalling the
/// caller to skip pushing a card.
#[cfg(feature = "terminal-tui")]
fn collect_reasoning_from_history_slice(slice: &[ChatMessage]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in slice {
        if msg.role != "assistant" {
            continue;
        }
        // Fast pre-filter to skip plain-text assistant turns without paying
        // the JSON parse cost.
        if !msg.content.contains("reasoning_content") {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&msg.content) else {
            continue;
        };
        if let Some(rc) = parsed.get("reasoning_content").and_then(serde_json::Value::as_str) {
            let trimmed = rc.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    parts.join("\n\n")
}

// ── P3-2: TerminalGuard RAII + strengthened panic hook ──────────────────────

/// Best-effort terminal restoration used by both [`TerminalGuard`] (on Drop /
/// manual `leave`) and the chat panic hook installed via
/// [`install_chat_panic_hook`].
///
/// Sequence (reverse of entry):
///   1. `DisableBracketedPaste` (paired with `EnableBracketedPaste` in enter)
///   2. Show cursor (explicit — `Frame::set_cursor_position` may have
///      left the cursor hidden if a panic happened mid-frame)
///   3. `disable_raw_mode`
///
/// P3-inline note: we no longer toggle the alternate screen, so there is
/// nothing to `LeaveAlternateScreen` here. Permanent chat output lives in
/// the host terminal's main scrollback (pushed via
/// `terminal.insert_before`); leaving raw mode is sufficient to give the
/// shell a usable cursor back after exit. The previous fullscreen design
/// wiped the screen on exit — the inline design intentionally preserves
/// it so users can review the conversation.
///
/// Every step swallows its error: by the time this runs we are already on the
/// cleanup path (Drop or panic unwind) and there is no caller left to surface
/// the failure to. Errors are silently dropped — logging is intentionally
/// avoided to keep this callable from a panic hook without re-entering the
/// tracing machinery.
fn restore_terminal_state() {
    // 1. Disable bracketed paste so the host shell is not left in a
    //    half-enabled state.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    // 2. Show cursor (idempotent — defends against a panic interrupting
    //    a frame that had hidden the cursor via `set_cursor_position`).
    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
    // 3. Disable raw mode last so any escape sequences emitted above are
    //    interpreted by the terminal as expected.
    let _ = crossterm::terminal::disable_raw_mode();
}

/// RAII guard for the chat TUI terminal state.
///
/// Owns the entry side-effects (`enable_raw_mode` + bracketed paste) and
/// guarantees they are reversed exactly once on Drop — whether by normal
/// return, `?` early-exit, or panic unwinding. The strengthened panic
/// hook in [`install_chat_panic_hook`] provides defence-in-depth for
/// non-unwind aborts and for panics that happen before a guard exists.
///
/// **P3-inline change.** This guard no longer enters the alternate
/// screen. Permanent conversation history is pushed to the host
/// terminal's main scrollback via `terminal.insert_before` (driven by
/// the unified TUI loop), and ratatui draws only a fixed-height inline
/// viewport at the bottom. The benefits are:
///   * the host terminal's native scroll (mouse wheel, Shift+PgUp,
///     terminal search / copy / paste) works on chat history;
///   * exiting prx leaves the conversation in the user's scrollback
///     instead of wiping the screen.
///
/// `enter()` is *transactional*: if bracketed paste fails after raw mode
/// succeeded, raw mode is rolled back before returning `Err`, so a
/// failed enter never leaves the terminal in a half-modified state.
///
/// The `alt_screen_active` flag is retained for source compatibility
/// with the existing teardown order (and to keep the unit tests
/// stable) but now corresponds to "bracketed paste is currently on"
/// rather than "alt screen is currently on".
pub struct TerminalGuard {
    /// True while raw mode is currently enabled by *this* guard.
    raw_mode_active: std::sync::atomic::AtomicBool,
    /// True while bracketed paste + cursor-show state is currently owned
    /// by *this* guard. (Pre-P3-inline this flag also tracked the
    /// alternate screen; the field name is kept for source compat.)
    alt_screen_active: std::sync::atomic::AtomicBool,
}

impl TerminalGuard {
    /// Enter raw mode + bracketed-paste mode.
    ///
    /// Note (P3-inline): we do **not** `EnterAlternateScreen` and we do
    /// **not** `cursor::Hide`. ratatui's `Frame::set_cursor_position`
    /// controls cursor visibility per frame; calling `Hide` ahead of
    /// time leaves `set_cursor_position` with no visible cursor to
    /// position and breaks the user's ability to see where their input
    /// is going. Bracketed paste is enabled so CJK IME committed strings
    /// arrive as a single `Event::Paste(s)` instead of being shredded
    /// into per-byte `KeyEvent`s with garbage modifier bits.
    ///
    /// Transactional: on partial failure (raw mode succeeded but
    /// bracketed paste failed) the partially-applied state is rolled
    /// back before returning `Err`, so callers never need to clean up
    /// after a failed `enter`.
    pub fn enter() -> Result<Self> {
        use std::sync::atomic::AtomicBool;

        // Step 1: raw mode.
        crossterm::terminal::enable_raw_mode()
            .map_err(|e| anyhow::anyhow!("failed to enable raw mode for chat TUI: {e}"))?;

        // Step 2: bracketed paste. If this fails, roll back step 1
        // before propagating the error. We intentionally do not enter
        // the alternate screen — see doc comment.
        if let Err(e) = crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste) {
            // Best-effort rollback — already on error path, ignore failure.
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(anyhow::anyhow!("failed to enable bracketed paste for chat TUI: {e}"));
        }

        Ok(Self {
            raw_mode_active: AtomicBool::new(true),
            alt_screen_active: AtomicBool::new(true),
        })
    }

    /// Manual early teardown (e.g. before spawning a child process that
    /// needs a clean terminal). Idempotent — subsequent calls (including the
    /// Drop hook) are a no-op.
    ///
    /// Uses two CAS operations so concurrent `leave()` / `drop()` from
    /// different threads is safe: only the first caller to flip the flag
    /// actually issues the crossterm calls.
    pub fn leave(&self) {
        use std::sync::atomic::Ordering;

        // Order mirrors the reverse of entry: disable bracketed paste +
        // show cursor first, then raw mode last. Notably we do NOT
        // clear the screen — the inline design leaves chat history in
        // the user's main scrollback so they can review it after exit.
        if self
            .alt_screen_active
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
            let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
        }
        if self
            .raw_mode_active
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.leave();
    }
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
fn install_chat_panic_hook() {
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

/// RAII guard owning the `tracing_appender` non-blocking worker. When dropped
/// it:
///   1. swaps the global tracing writer back to stderr (best-effort), so
///      any post-chat logs (e.g. shutdown errors) remain visible;
///   2. drops `_worker_guard`, which flushes and joins the appender thread.
///
/// Held for the lifetime of `chat::run`. If construction fails we keep the
/// existing stderr writer — no panics, no silent data loss.
pub(crate) struct TracingChatGuard {
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
        // _worker_guard drops next → tracing-appender flushes pending lines.
    }
}

/// Compute `~/.openprx/` (or `$HOME/.openprx/`) for the chat log directory.
fn resolve_chat_log_dir() -> Result<std::path::PathBuf> {
    if let Some(dirs) = directories::UserDirs::new() {
        return Ok(dirs.home_dir().join(".openprx"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(std::path::PathBuf::from(home).join(".openprx"));
    }
    anyhow::bail!("cannot determine home directory for chat.log")
}

/// Redirect the global `tracing` writer to `~/.openprx/chat.log` so
/// `INFO`/`WARN`/`ERROR` lines never collide with the ratatui TUI.
///
/// Returns a guard that MUST be held for the lifetime of the chat session.
/// On any failure (no HOME, dir not creatable, file not openable, no global
/// reload handle) returns `Err` without panicking — callers fall back to the
/// existing stderr writer.
pub(crate) fn setup_chat_tracing_to_file() -> Result<TracingChatGuard> {
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

    Ok(TracingChatGuard {
        _worker_guard: worker_guard,
    })
}

/// Run the interactive chat session with rich terminal UI.
#[allow(clippy::too_many_lines)]
pub async fn run(
    config: Config,
    provider_override: Option<String>,
    model_override: Option<String>,
    temperature: f64,
    plain_mode: bool,
    session_id: Option<String>,
    list_sessions: bool,
) -> Result<()> {
    // ── P3-1: Redirect tracing to ~/.openprx/chat.log ────────────────────
    // Held for the rest of this function. On failure (no HOME, log dir
    // unwritable, etc.) we fall back to the stderr writer that `main` set up
    // and emit a warning. Never panics.
    let _tracing_guard: Option<TracingChatGuard> = match setup_chat_tracing_to_file() {
        Ok(g) => Some(g),
        Err(e) => {
            tracing::warn!(error = %e, "P3-1: keeping tracing on stderr (chat.log unavailable)");
            None
        }
    };

    // ── Panic hook: restore terminal state on crash ─────────────────────
    // Strengthened in P3-2: restoration runs before the chained hook so
    // backtraces print to a usable terminal. Idempotent across multiple
    // calls (OnceLock-guarded).
    install_chat_panic_hook();

    // P3-2 prep: `TerminalGuard::enter()` is intentionally NOT invoked from
    // this function yet. It will be wired up in P3-3 once the ratatui draw
    // loop replaces the reedline / UiActor path. The type is defined and the
    // panic hook is strengthened ahead of time so P3-3 can plug in without
    // any scaffolding changes — see `TerminalGuard` above.

    // ── Wire up subsystems (same as agent::run) ──────────────────
    let base_observer = observability::create_observer(&config.observability);
    let observer: Arc<dyn Observer> = Arc::from(base_observer);
    let hooks = Arc::new(HookManager::new(config.workspace_dir.clone()));
    let runtime: Arc<dyn runtime::RuntimeAdapter> = Arc::from(runtime::create_runtime(&config.runtime)?);
    let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));

    // ── Memory ───────────────────────────────────────────────────
    let mem: Arc<dyn Memory> = Arc::from(memory::create_memory_with_storage_and_routes_with_acl(
        &config.memory,
        &config.embedding_routes,
        Some(&config.storage.provider.config),
        &config.workspace_dir,
        config.api_key.as_deref(),
        &config.identity_bindings,
        &config.user_policies,
    )?);
    info!(backend = mem.name(), "Memory initialized");

    // ── List sessions (early return) ─────────────────────────────
    if list_sessions {
        return list_saved_sessions(mem.as_ref()).await;
    }

    // ── Tools ────────────────────────────────────────────────────
    let (composio_key, composio_entity_id) = if config.composio.enabled {
        (
            config.composio.api_key.as_deref(),
            Some(config.composio.entity_id.as_str()),
        )
    } else {
        (None, None)
    };
    // 5a-6: tools_registry 用 `Arc<Vec<Box<dyn Tool>>>` 共享，让 Redux driver
    // (Effect::StartTurn → drive_start_turn_stream 子任务) 与 legacy `run_tool_call_loop`
    // (借用 &tools_registry) 共用同一份 registry，无需重新构造。Arc clone 仅 +1 引用，
    // 不触发深拷贝；legacy 借用通过 &*tools_registry 解引用拿到 &Vec.
    let tools_registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(tools::all_tools_with_runtime(
        Arc::new(config.clone()),
        &security,
        runtime,
        mem.clone(),
        composio_key,
        composio_entity_id,
        &config.browser,
        &config.http_request,
        &config.workspace_dir,
        &config.agents,
        config.api_key.as_deref(),
        &config,
    ));

    // ── Resolve provider ─────────────────────────────────────────
    let provider_name = provider_override
        .as_deref()
        .or(config.default_provider.as_deref())
        .unwrap_or("openrouter");

    let model_name = model_override
        .as_deref()
        .or(config.default_model.as_deref())
        .unwrap_or("anthropic/claude-sonnet-4");

    let provider_runtime_options = providers::ProviderRuntimeOptions {
        auth_profile_override: None,
        openprx_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        codex_auth_json_path: Some(config.auth.codex_auth_json_path.clone()),
        codex_auth_json_auto_import: config.auth.codex_auth_json_auto_import,
        reasoning_enabled: config.runtime.reasoning_enabled,
        codex_stream_idle_timeout_secs: config.runtime.codex_stream_idle_timeout_secs,
        codex_reasoning_effort: config.runtime.codex_reasoning_effort.clone(),
    };

    let provider: Arc<dyn Provider> = Arc::from(providers::create_routed_provider_with_options(
        provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &config.model_routes,
        model_name,
        &provider_runtime_options,
    )?);

    observer.record_event(&ObserverEvent::AgentStart {
        provider: provider_name.to_string(),
        model: model_name.to_string(),
    });
    hooks
        .emit(
            HookEvent::AgentStart,
            serde_json::json!({
                "provider": provider_name,
                "model": model_name,
            }),
        )
        .await;

    // ── Skills ────────────────────────────────────────────────────
    let skill_embedder = memory::create_embedder_from_config(&config, config.api_key.as_deref());
    let mut skills = crate::skills::load_skills_with_config(&config.workspace_dir, &config);
    if config.skill_rag.enabled {
        crate::skills::hydrate_skill_embeddings(&mut skills, skill_embedder.as_ref()).await?;
    }

    // ── Tool descriptions for system prompt ──────────────────────
    let tool_descs: Vec<(&str, &str)> = vec![
        ("shell", "Execute terminal commands."),
        ("file_read", "Read file contents."),
        ("file_write", "Write file contents."),
        ("memory_store", "Save to memory."),
        ("memory_recall", "Search memory."),
        ("memory_forget", "Delete a memory entry."),
    ];
    let native_tools = provider.supports_native_tools();

    // ── Approval manager ─────────────────────────────────────────
    let approval_manager = ApprovalManager::from_config(&config.autonomy);

    // ── Create TerminalChannel (Arc-wrapped for sharing with streaming tasks) ──
    let terminal: Arc<TerminalChannel> = Arc::new(TerminalChannel::new(plain_mode));

    // ── Session: resume or create new ───────────────────────────
    let mut chat_session = match session_id.as_deref() {
        Some("last") => match load_latest_session(mem.as_ref()).await {
            Some(s) => {
                info!(id = %s.id, title = %s.title, turns = s.turn_count(), "Resumed session");
                s
            }
            None => {
                info!("No previous session found, starting new");
                session::ChatSession::new(provider_name, model_name)
            }
        },
        Some(id) => match load_session_by_id(mem.as_ref(), id).await {
            Some(s) => {
                info!(id = %s.id, title = %s.title, turns = s.turn_count(), "Resumed session");
                s
            }
            None => {
                eprintln!("Session '{id}' not found, starting new session.");
                session::ChatSession::new(provider_name, model_name)
            }
        },
        None => session::ChatSession::new(provider_name, model_name),
    };

    // ── Build banner text ────────────────────────────────────────
    // On the TUI path the banner is *not* printed to stdout here — printing
    // before `TerminalGuard::enter()` would pollute the parent shell's
    // scrollback, and printing after would corrupt the ratatui draw buffer
    // (raw mode strips `\r` from `\n`, producing ladder-shaped garbage).
    // Instead we capture it as a `String` and `push_system_message` it into
    // the shared `chat_mirror` after the guard is in place but before the
    // unified render loop spawns. The legacy fallback path (no TUI) prints
    // the banner the old way.
    // Claude-Code style single-line minimal banner: `prx <version> · provider/model`.
    // The previous multi-line "PRX Chat — ... \n Type /help for commands"
    // hint has moved to the persistent footer instead. `chat_session` is no
    // longer queried for the banner but stays in scope for downstream use.
    let banner = format!(
        "prx {} \u{00B7} {provider_name}/{model_name}",
        env!("CARGO_PKG_VERSION")
    );

    // ── Conversation history ─────────────────────────────────────
    let mut history = if config.skill_rag.enabled {
        Vec::new()
    } else {
        vec![ChatMessage::system(build_runtime_system_prompt(
            &config,
            model_name,
            &tool_descs,
            &skills,
            native_tools,
            &tools_registry,
        ))]
    };

    // ── P3-3: shared TuiState mirror ─────────────────────────────
    //
    // A single `Arc<parking_lot::Mutex<TuiState>>` is bound to `chat::run`'s
    // lifetime and threaded into every producer that wants to mutate the
    // visible TUI state: the input task (keystrokes, history navigation),
    // the per-turn tool-event forwarder (`push_tool_result_*`), the reasoning
    // push at end-of-turn, and — once P3-4 lands — the UiActor draft/stream
    // bridge. The render task (also spawned here on the TUI path) only
    // **reads** the mirror under a short-lived lock.
    //
    // Replacing the previous two-instance design (`mirror_for_input` +
    // per-turn `tui_mirror`) collapses all observable mutations into a
    // single state machine so the renderer sees a consistent view.
    #[cfg(feature = "terminal-tui")]
    let chat_mirror: Arc<parking_lot::Mutex<tui::TuiState>> =
        Arc::new(parking_lot::Mutex::new(tui::TuiState::new(provider_name, model_name)));

    // ── Input channel ────────────────────────────────────────────
    let (input_tx, mut input_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);

    // ── Graceful shutdown signal ─────────────────────────────────
    // Instead of std::process::exit(), all signal handlers use this token to
    // break the main loop gracefully, allowing final session save + teardown.
    // Created up here (earlier than before) so the TUI input task can also
    // observe shutdown and exit its blocking poll cleanly.
    let shutdown = CancellationToken::new();

    // ── Step 5a-1: Redux dispatcher (shadow / real-deps mode) ────
    //
    // 全局 dispatcher channel + EffectExecutor + ChatState 在此构造。
    // EffectExecutor 模式由 `PRX_CHAT_REDUX` env 决定：
    //   - Off (默认)：shadow 模式，业务 Effect 全部 no-op；旧路径单写
    //   - Both：real 模式，业务 Effect 真执行 + 旧路径仍跑；dual_write_guard
    //     在 reducer 持久化 effect 后置位，旧路径检查 guard 跳过对应写
    //   - Redux：与 Both 行为相同（5a-1 阶段不删旧路径；5a-3 才真正删旧路径）
    //
    // bounded(2048)：覆盖典型 chat session 的 Action 流（用户输入 + 流式 chunk +
    // 工具事件 + 信号），同时在反压时通过 [`StreamChunkCoalescer`] 合并 delta，
    // 避免无界增长导致 OOM。
    let (chat_dispatcher, chat_action_rx) = dispatcher::ChatDispatcher::new();
    let dispatcher_shadow_state =
        state::ChatState::new(Arc::from(provider_name), Arc::from(model_name), shutdown.clone());

    // 共享 dual-write guard（在 Both/Redux 模式下被 EffectExecutor 置位；旧路径
    // 检查 guard 决定是否跳过持久化。即使 Off 模式也构造，旧路径检查总是 false 零开销。
    // P0-1 fix: 去掉 allow(unused_variables)，guard 在旧路径 turn 结束时被读取，
    // 两种 feature 配置下都确保真正使用）
    let dual_write_guard = dispatcher::RuntimeDualWriteGuard::new();

    // 根据 redux mode 选择 EffectExecutor 模式（TUI feature only）
    #[cfg(feature = "terminal-tui")]
    let effect_executor = {
        let mode = ReduxMode::from_env();
        if matches!(mode, ReduxMode::Off) {
            dispatcher::EffectExecutor::new_shadow()
        } else {
            let deps = dispatcher::EffectDeps {
                provider: Arc::clone(&provider),
                memory: Arc::clone(&mem),
                channel: Arc::clone(&terminal) as Arc<dyn crate::channels::Channel>,
                hooks: Arc::clone(&hooks),
                observer: Arc::clone(&observer),
                action_tx: chat_dispatcher.sender(),
                dual_write_guard: dual_write_guard.clone(),
                redraw_tx: None,
                shutdown: shutdown.clone(),
                model: Arc::from(model_name),
                temperature,
                // 5a-6: 共享 tools_registry，让 driver 在 tool turn 中按名查表执行。
                tools_registry: Some(Arc::clone(&tools_registry)),
                max_tool_iterations: config.agent.max_tool_iterations,
                // S3 T3-1: approval 桥接 — 共享 router + manager 句柄给 driver。
                // ApprovalManager 由 chat::run 上方已构造（line 814），这里 wrap Arc。
                approval_router: Arc::new(dispatcher::ApprovalRouter::new()),
                approval_manager: Some(Arc::new(ApprovalManager::from_config(&config.autonomy))),
            };
            tracing::info!(mode = ?mode, "PRX_CHAT_REDUX: EffectExecutor in real-deps mode");
            dispatcher::EffectExecutor::new_with_deps(deps)
        }
    };
    #[cfg(not(feature = "terminal-tui"))]
    let effect_executor = dispatcher::EffectExecutor::new_shadow();

    // P0-2 fix: 提前获取 redraw_slot Arc，用于在 TUI 初始化完成后后注入 redraw_tx。
    // EffectExecutor 被 spawn_dispatcher_task_with_executor 消费，但 Arc 在 spawn
    // 前复制出来，spawn 后仍可通过此 Arc 填入真实 sender，让 RequestRedraw 真执行。
    #[cfg(feature = "terminal-tui")]
    let executor_redraw_slot = effect_executor.redraw_handle();

    // Step 5a-4: TurnCompletionSignal — Redux driver 切闸路径用此 signal 在
    // chat::run 主循环里 await turn 完成。dispatcher task 消费 terminal action
    // (StreamCompleted/Failed/Cancelled) 后 notify_waiters，唤醒等待。
    // Off / legacy 路径不读 signal，构造成本极低（Arc<Notify>）。
    let turn_signal = dispatcher::TurnCompletionSignal::new();

    let dispatcher_handle = dispatcher::spawn_dispatcher_task_with_signal(
        dispatcher_shadow_state,
        chat_action_rx,
        shutdown.clone(),
        effect_executor,
        Some(turn_signal.clone()),
    );

    // ── Ctrl+C shared state ─────────────────────────────────────
    // Tracks the timestamp (ms) of the last Ctrl+C press for double-press detection.
    // Lifted above the input loop so the TUI dispatcher can fold its own
    // Ctrl+C presses into the same double-press → exit semantics.
    let last_ctrlc_ms = Arc::new(AtomicU64::new(0));
    // The active cancellation token for the current generation turn (if any).
    let active_cancel: Arc<parking_lot::Mutex<Option<CancellationToken>>> = Arc::new(parking_lot::Mutex::new(None));

    // Spawn the appropriate input loop:
    //   - feature `terminal-tui` + TTY stdin + (PRX_TUI != "0") → ratatui/crossterm
    //     KeyEvent loop driving `dispatch_global_key` against the shared
    //     `chat_mirror`, plus a `spawn_render_task` that owns the
    //     `ratatui::Terminal` and redraws on demand.
    //   - otherwise → legacy reedline + BufRead fallback via TerminalChannel.
    //
    // `_terminal_guard` is bound to this function's stack so its Drop runs at
    // chat::run exit (panic-safe via `install_chat_panic_hook` above). The
    // legacy path leaves `_terminal_guard = None`, so no entry side-effects
    // are applied.
    // `redraw_tx_for_main` is `Some(sender)` only on the TUI path; the main
    // loop uses it to nudge the renderer after mutating `chat_mirror` (e.g.
    // echoing the user's submitted input so the conversation pane reflects
    // it immediately rather than waiting for the next async event).
    #[cfg(feature = "terminal-tui")]
    let (_terminal_guard, redraw_tx_for_main): (Option<TerminalGuard>, Option<mpsc::Sender<()>>) = {
        use std::io::IsTerminal as _;
        // TUI is on by default in TTY. Opt out with PRX_TUI=0 (e.g. for
        // downstream scripts that scrape stdout, or to escape rendering
        // glitches). Non-TTY stdin (pipe / heredoc / scripted) always falls
        // through to the legacy reedline + BufRead path.
        let tui_opt_out = std::env::var("PRX_TUI").as_deref() == Ok("0");
        let tui_enabled = !tui_opt_out && std::io::stdin().is_terminal();
        if tui_enabled {
            // Order matters: `TerminalGuard::enter()` flips raw mode + alt
            // screen + bracketed paste FIRST, then we wire up the UiActor
            // mirror BEFORE spawning the unified TUI loop (so no `UiEvent`
            // can sneak through to the old println!-based renderer in
            // `channels/terminal.rs`). On enter failure we fall back to the
            // legacy reedline path so the user is never left without a
            // prompt.
            match TerminalGuard::enter() {
                Ok(guard) => {
                    // Seed the banner into the mirror state *before* the
                    // first draw, so the user sees it on entry rather than
                    // having it print to the parent shell's scrollback.
                    chat_mirror.lock().push_system_message(&banner);
                    // S2-C Step 3: 双写到 Redux UI 镜像。chat_mirror 仍是 TUI 渲染源
                    // （Codex P0-3：不能用 reducer ui.conversation_lines 替代 mirror，
                    // 真实可见 TUI 由 mirror 主导）。本 dispatch 仅供 Redux 路径维护
                    // 一致的 UI 账本 + 测试断言，redraw_tx 此时尚未注入 EffectExecutor
                    // 故 RequestRedraw 是 no-op，下方 spawn_tui_unified_loop 启动后
                    // 首屏会自然 redraw。
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::SystemMessageAdded { text: banner.clone() },
                        "chat.banner",
                    );

                    // The redraw channel exists solely so the UiActor and
                    // background tasks can wake the unified loop on
                    // streaming events. cap=1 + try_send is the coalesce
                    // idiom: bursts collapse into a single deferred redraw.
                    let (redraw_tx, redraw_rx) = mpsc::channel::<()>(1);

                    // P0-2 fix: 将 redraw_tx 后注入 EffectExecutor 的 redraw_slot。
                    // EffectExecutor 已被 dispatcher task 消费，但通过提前保存的
                    // executor_redraw_slot Arc 可跨越 spawn 边界填入真实 sender，
                    // 从而让 RequestRedraw effect 真正触发重绘。
                    *executor_redraw_slot.lock() = Some(redraw_tx.clone());
                    tracing::debug!("P0-2: redraw_tx injected into EffectExecutor redraw_slot");

                    // P3-4: route every UiActor event into the shared TUI
                    // mirror instead of writing to stdout (which would tear
                    // the draw loop). Must run BEFORE the unified loop
                    // starts taking events.
                    let sink = Box::new(tui::TuiStateMirrorSink::new(Arc::clone(&chat_mirror)));
                    terminal.with_tui_mirror(sink, redraw_tx.clone()).await;

                    // P3-rearch: single thread owns Terminal/stdout + reads
                    // crossterm events. No more spawn_render_task /
                    // spawn_redraw_tick_task / spawn_tui_input_task trio —
                    // they fought each other over the same stdout handle.
                    // Hand a clone to the main loop so it can request a
                    // redraw immediately after echoing the user's input
                    // into `chat_mirror`.
                    let redraw_tx_main = redraw_tx.clone();
                    let redraw_tx_loop = redraw_tx.clone();
                    spawn_tui_unified_loop(
                        input_tx,
                        Arc::clone(&chat_mirror),
                        redraw_rx,
                        redraw_tx_loop,
                        shutdown.clone(),
                        Arc::clone(&last_ctrlc_ms),
                        Arc::clone(&active_cancel),
                        chat_dispatcher.clone(),
                    );
                    (Some(guard), Some(redraw_tx_main))
                }
                Err(e) => {
                    tracing::warn!(error = %e, "TerminalGuard::enter failed; falling back to reedline input");
                    // On guard failure the banner has not been printed yet
                    // and we are about to use the legacy non-TUI path, so
                    // print it the old way.
                    println!("{banner}");
                    let terminal_for_listen = TerminalChannel::new(plain_mode);
                    tokio::spawn(async move {
                        if let Err(e) = terminal_for_listen.listen(input_tx).await {
                            tracing::error!("Terminal input loop error: {e}");
                        }
                    });
                    (None, None)
                }
            }
        } else {
            // Fallback path (PRX_TUI=0 opt-out, or non-TTY pipe/heredoc) — keep
            // the legacy reedline + BufRead fallback via TerminalChannel and
            // print the banner the old way for parity with the previous
            // behaviour.
            println!("{banner}");
            let terminal_for_listen = TerminalChannel::new(plain_mode);
            tokio::spawn(async move {
                if let Err(e) = terminal_for_listen.listen(input_tx).await {
                    tracing::error!("Terminal input loop error: {e}");
                }
            });
            (None, None)
        }
    };
    #[cfg(not(feature = "terminal-tui"))]
    {
        println!("{banner}");
        let terminal_for_listen = TerminalChannel::new(plain_mode);
        tokio::spawn(async move {
            if let Err(e) = terminal_for_listen.listen(input_tx).await {
                tracing::error!("Terminal input loop error: {e}");
            }
        });
    }

    // Persistent Ctrl+C handler: runs for the entire chat session.
    // - If a generation is active: cancel it (first press) or exit (double press).
    // - If idle (no generation): exit on double press.
    //
    // Step 5b 双写：每次 Ctrl+C 在旧路径 cancel/shutdown 之外，同步 try_dispatch
    // `CancelRequested` / `ShutdownRequested` 给 dispatcher（shadow 模式下仅入 reducer
    // + log，不参与真实控制流）。try_send 满或 closed 都不影响旧路径兜底。
    //
    // shutdown 触发时 handler 也需要退出，避免持有 dispatcher sender 阻塞
    // dispatcher task 退出（drop(chat_dispatcher) + 此 handler 内的 clone 同时
    // drop，channel 才能真正关闭，dispatcher_handle.await 才能返回）。
    {
        let last_ctrlc = Arc::clone(&last_ctrlc_ms);
        let cancel_ref = Arc::clone(&active_cancel);
        let shutdown_signal = shutdown.clone();
        let dispatcher_for_signal = chat_dispatcher.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    () = shutdown_signal.cancelled() => break,
                    res = tokio::signal::ctrl_c() => {
                        if res.is_err() {
                            break;
                        }
                    }
                }
                let now = now_ms();
                let prev = last_ctrlc.swap(now, Ordering::Relaxed);

                if now.saturating_sub(prev) < DOUBLE_CTRLC_WINDOW_MS {
                    // Double Ctrl+C → graceful shutdown
                    eprintln!("\nExiting...");
                    // Step 5b shadow: 同步投递 ShutdownRequested.
                    let _ = dispatcher_for_signal.dispatch_or_log(
                        crate::chat::action::Action::ShutdownRequested,
                        "chat.shutdown_double_ctrlc",
                    );
                    shutdown_signal.cancel();
                    break;
                }

                // Single Ctrl+C → cancel active generation if any
                // Step 5b shadow: 同步投递 CancelRequested 给 reducer 观察。
                let _ = dispatcher_for_signal
                    .dispatch_or_log(crate::chat::action::Action::CancelRequested, "chat.cancel_single_ctrlc");
                if let Some(token) = cancel_ref.lock().as_ref() {
                    token.cancel();
                }
            }
        });
    }

    // SIGTERM handler: signal graceful shutdown.
    //
    // Step 5b 双写：投递 ShutdownRequested 给 dispatcher（shadow 观察），同时
    // 调用 shutdown.cancel() 兜底（旧路径退出协议保留）。
    // shutdown 触发时此任务也要主动退出，避免持有 sender clone 阻塞 dispatcher
    // task 关闭。
    #[cfg(unix)]
    {
        let sigterm_result = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
        match sigterm_result {
            Ok(mut sigterm) => {
                let shutdown_signal = shutdown.clone();
                let dispatcher_for_sigterm = chat_dispatcher.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        biased;
                        () = shutdown_signal.cancelled() => {
                            // 主路径已 shutdown，无需再触发；退出释放 sender clone。
                        }
                        _ = sigterm.recv() => {
                            let _ = dispatcher_for_sigterm
                                .dispatch_or_log(crate::chat::action::Action::ShutdownRequested, "chat.shutdown_sigterm");
                            shutdown_signal.cancel();
                        }
                    }
                });
            }
            Err(e) => {
                tracing::warn!("Failed to register SIGTERM handler: {e}");
            }
        }
    }

    // ── Main message loop ────────────────────────────────────────
    while let Some(msg) = tokio::select! {
        msg = input_rx.recv() => msg,
        _ = shutdown.cancelled() => None,
    } {
        let user_input = msg.content.clone();

        // Step 5b 双写：每条用户输入入 dispatcher（shadow 观察 reducer）。
        // InputSubmitted 仅记 UI/LogTrace；RecordUserTurn 真写 history + session.turns，
        // 必须在 mem_context 注入后才 dispatch（用 `enriched` 与 legacy `history.push`
        // 字节级对齐 — 见 S2-B Step 4 risk notes）.
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::InputSubmitted(user_input.clone()),
            "chat.input_submitted",
        );

        // Echo the user's input into the TUI conversation pane.
        //
        // Why: in raw-mode TUI the input box clears on submit, so without
        // this push the user has no visual confirmation of what they sent
        // until the assistant streams its reply. We push BEFORE the slash
        // command short-circuits below so that even `/quit`, `/clear`, etc.
        // produce a visible record of what was typed.
        //
        // Non-TUI (`--plain`, piped stdin, reedline fallback) is unaffected:
        // the terminal already echoes typed characters as cooked input, and
        // `redraw_tx_for_main` is `None` on those paths.
        #[cfg(feature = "terminal-tui")]
        {
            chat_mirror.lock().push_user_message(&user_input);
            if let Some(tx) = redraw_tx_for_main.as_ref() {
                // cap=1 + try_send: bursts coalesce into a single deferred
                // redraw — the unified loop will pick up the latest state.
                let _ = tx.try_send(());
            }
        }

        // Handle /quit and /exit immediately
        if matches!(user_input.as_str(), "/quit" | "/exit") {
            break;
        }

        // Route any user-visible slash-command output into the right sink:
        // ratatui mirror on the TUI path (so it survives raw-mode `\n`
        // mangling), plain stdout otherwise. Returns immediately for plain
        // mode so the legacy `--plain` / piped path is unchanged.
        let emit_chat_output = |text: &str| {
            #[cfg(feature = "terminal-tui")]
            {
                chat_mirror.lock().push_system_message(text);
                // S2-C Step 3: 双写到 Redux UI 镜像（chat_mirror 仍是渲染源）。
                // Codex P0-3：reducer ui.conversation_lines 不能替代 mirror，
                // 因此 mirror 写不能在 S2-C 阶段加守卫关闭；本 dispatch 仅是观察账本。
                let _ = chat_dispatcher.dispatch_or_log(
                    crate::chat::action::Action::SystemMessageAdded { text: text.to_string() },
                    "chat.system_message_slash",
                );
                // Nudge the unified loop so the slash-command echo shows up
                // immediately rather than waiting up to 50 ms for the next
                // crossterm poll cycle. `try_send` + cap=1 coalesces bursts.
                if let Some(tx) = redraw_tx_for_main.as_ref() {
                    let _ = tx.try_send(());
                }
            }
            #[cfg(not(feature = "terminal-tui"))]
            {
                println!("{text}");
            }
        };

        // Handle /clear separately (needs mutable history)
        if matches!(user_input.as_str(), "/clear" | "/new") {
            history.clear();
            // S2-C Step 4: 双写 HistoryCleared 到 reducer。reducer 的语义是
            // "drain 所有非 system + 保留 system"——legacy 是先 clear 再可能 push
            // system（仅当 !skill_rag.enabled），最终态都是 "system only"（或空，
            // 当 skill_rag.enabled 时）。双写期两路径终态一致，但中间状态不同：
            //   - legacy: clear() 把 history 清空 → 可能 push system
            //   - reducer: HistoryCleared 保留已有 system（不重新构造）
            // 实际生产路径 legacy 后续会 push 新构造的 system（覆盖旧 system 的
            // skill 列表），reducer 这边的 system 仍是上一轮的。本 S2-C 阶段
            // 不做修正——legacy 仍是 LLM 真上下文源，reducer 是观察账本。
            let _ =
                chat_dispatcher.dispatch_or_log(crate::chat::action::Action::HistoryCleared, "chat.history_cleared");
            if !config.skill_rag.enabled {
                let cleared_system = build_runtime_system_prompt(
                    &config,
                    model_name,
                    &tool_descs,
                    &skills,
                    native_tools,
                    &tools_registry,
                );
                history.push(ChatMessage::system(cleared_system.clone()));
                // S2-C Step 4 (Codex P0 修正): 用 SetLeadingSystemPrompt 而非
                // RecordSystemMessage。reducer 的 HistoryCleared 是 "drain 非 system
                // 保留 system" — 之前的 system 仍在；若此处用 RecordSystemMessage
                // (append) 会产生重复 system，长期累计多条。SetLeadingSystemPrompt
                // 是 upsert：替换已有首位 system 或 push 到空 history，与 legacy
                // `clear + push` 终态等价（≤ 1 条 system）。
                let _ = chat_dispatcher.dispatch_or_log(
                    crate::chat::action::Action::SetLeadingSystemPrompt {
                        content: cleared_system,
                    },
                    "chat.system_prompt_after_clear",
                );
            }
            let cleared = commands::handle_clear(mem.as_ref(), Some(&chat_session.id)).await;
            let msg = if cleared > 0 {
                format!("Conversation cleared ({cleared} memory entries removed).")
            } else {
                "Conversation cleared.".to_string()
            };
            emit_chat_output(&msg);
            continue;
        }

        // Dispatch other slash commands
        {
            let cmd_ctx = commands::CommandContext {
                model_name,
                provider_name,
                chat_session: &chat_session,
                tools_registry: &tools_registry,
                mem: mem.as_ref(),
            };
            match commands::dispatch(&user_input, &cmd_ctx).await {
                commands::CommandResult::Handled => continue,
                commands::CommandResult::HandledWithOutput(text) => {
                    emit_chat_output(&text);
                    continue;
                }
                commands::CommandResult::Quit => break,
                commands::CommandResult::SetMode(mode) => {
                    // S2-B Step 4: dispatch `Action::ModeChanged` 让 reducer 写
                    // `state.session.mode`。T3-3-fixB B4：Pure 模式跳过 legacy
                    // `chat_session.set_mode`——Pure 下 chat_session 不参与持久化
                    // (SaveSession 走 reducer 快照)，写 mode 是死写。Off/Both/Redux
                    // 仍需 legacy 写，run_tool_call_loop 仍读 chat_session.mode。
                    let _ = chat_dispatcher
                        .dispatch_or_log(crate::chat::action::Action::ModeChanged(mode), "chat.mode_changed");
                    #[cfg(feature = "terminal-tui")]
                    let legacy_session_mode_writes_enabled = !ReduxMode::from_env().is_pure();
                    #[cfg(not(feature = "terminal-tui"))]
                    let legacy_session_mode_writes_enabled = true;
                    if legacy_session_mode_writes_enabled {
                        chat_session.set_mode(mode);
                    }
                    let msg = match mode {
                        commands::ChatMode::Plan => {
                            "Switched to plan mode (read-only tools only — write/shell/git_commit will be simulated)"
                        }
                        commands::ChatMode::Edit => "Switched to edit mode (default — write tools enabled)",
                        commands::ChatMode::Auto => "Switched to auto mode (all tools, no approval prompts)",
                    };
                    emit_chat_output(msg);
                    continue;
                }
                commands::CommandResult::NotACommand => {}
            }
        }

        // Auto-save user message to memory
        if config.memory.auto_save
            && user_input.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS
            && memory::should_autosave_content(&user_input)
        {
            let user_key = autosave_memory_key("user_msg");
            let _ = mem
                .store(&user_key, &user_input, MemoryCategory::Conversation, None)
                .await;
        }

        // Inject memory context
        let mem_context = build_context(mem.as_ref(), &user_input, config.memory.min_relevance_score).await;
        let context = mem_context.preamble.clone();
        let enriched = if context.is_empty() {
            user_input.clone()
        } else {
            format!("{context}{user_input}")
        };

        // Build system prompt with skill selection
        let selected_skills = select_prompt_skills(&user_input, &skills, &config, skill_embedder.as_ref()).await;
        let system_prompt = build_runtime_system_prompt(
            &config,
            model_name,
            &tool_descs,
            &selected_skills,
            native_tools,
            &tools_registry,
        );
        if history.is_empty() {
            history.push(ChatMessage::system(system_prompt.clone()));
        } else if let Some(first) = history.first_mut() {
            *first = ChatMessage::system(system_prompt.clone());
        }
        // S2-C Step 4: 双写 SetLeadingSystemPrompt 到 reducer — 与 legacy
        // `if empty { push } else { first_mut = ... }` 字节级语义对齐（reducer
        // 内部走同样分支）。每轮 turn 都会跑，append 表达会让 system 堆积。
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::SetLeadingSystemPrompt { content: system_prompt },
            "chat.system_prompt_per_turn",
        );
        history.push(ChatMessage::user(&enriched));

        // S2-B Step 4: 在与 legacy `history.push(ChatMessage::user(&enriched))` 同一点
        // dispatch `RecordUserTurn(enriched)` — reducer 内 session.history 与 legacy
        // history 字节级对齐，session.turns 也用 enriched（与 legacy
        // `chat_session.add_user_turn(&sanitized_input)` 略有 sanitization 差异；
        // S2-C 阶段统一持久化路径时再合并）。
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::RecordUserTurn(enriched.clone()),
            "chat.record_user_turn",
        );

        // ── Set active recipient/channel on tools (for proactive messaging) ──
        for tool in tools_registry.iter() {
            tool.set_active_recipient("user").await;
            tool.set_active_channel(Arc::clone(&terminal) as Arc<dyn Channel>).await;
        }

        // ── Streaming pipeline setup ─────────────────────────────
        //
        // The delta channel carries ONLY visible assistant text — never the
        // model's reasoning/thinking content. Reasoning is separated upstream
        // at the provider parsing layer (see `parse_native_response` /
        // `parse_sse_line` in providers/{anthropic,openai,ollama,compatible}.rs)
        // and travels back via `ProviderChatResponse.reasoning_content`. The
        // tool-call loop persists reasoning into conversation history through
        // `build_native_assistant_history`; the live stream below renders text
        // only, so the user never sees the model's internal monologue.
        let cancellation = CancellationToken::new();
        let (delta_tx, delta_rx) = mpsc::channel::<String>(DELTA_CHANNEL_CAPACITY);

        // S2-A: per-turn Redux mode snapshot. Read once here so the
        // `draft_updater` closure (and any future per-turn gating) sees a
        // consistent view even if env changes mid-process. Off/Both keep the
        // legacy `update_draft(accumulated)` UI writer; pure Redux mode lets
        // the reducer-driven renderer own UI text exclusively.
        #[cfg(feature = "terminal-tui")]
        let redux_mode = ReduxMode::from_env();

        // Start a streaming draft on the terminal
        let draft_id = match terminal.send_draft(&SendMessage::new("", "user")).await {
            Ok(id) => id,
            Err(e) => {
                tracing::debug!("Failed to start draft: {e}");
                None
            }
        };

        // Step 5b 双写：宣告新一轮 LLM 推理开始（仅在 draft 存在时）。
        // shadow 模式下 reducer 设置 stream.draft + control.generating=true；
        // 无外部副作用（业务 Effect no-op）。
        if let Some(ref d_id) = draft_id {
            let _ = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::TurnStarted {
                    draft_id: d_id.clone(),
                    cancel: cancellation.clone(),
                },
                "chat.turn_started",
            );
        }

        // Spawn background task: accumulate deltas → channel.update_draft()
        // Follows the exact same pattern as process_channel_message in channels/mod.rs.
        //
        // P1-6 — Monotonic draft version protocol (Step 3 update). The
        // sender-side counter still stamps each accumulated snapshot with a
        // strictly monotonic `u64` (kept for `update_draft` downstream
        // consumers and the inline-redraw protocol). The receiver-side stale
        // check formerly performed here by `DraftVersionTracker.accept()` has
        // been DELETED — its protection is now owned end-to-end by the
        // Redux-style reducer in `chat::state` via `StreamState::draft.version`
        // (see `ChatState::reduce_stream_chunk_received`).
        //
        // Why this is safe to remove:
        //   1. `delta_rx` is a single-task tokio mpsc; FIFO is guaranteed at
        //      the runtime layer (no parallel accumulators).
        //   2. The counter is incremented atomically inside the same task,
        //      producing a strictly monotonic sequence by construction. The
        //      old tracker call was over-defence ("unreachable here" per the
        //      original comment).
        //   3. The reducer's `StreamChunkReceived` arm now enforces
        //      strict-monotonic version + draft_id matching + finalize-state
        //      check as the single source of truth. Once Step 5 makes the
        //      reducer the renderer source, this task disappears entirely.
        //
        // This is the P3-5 "one-shot switch" called out in the Step 3 plan:
        // no double-write period for the version protocol. Off / Both / Redux
        // modes all share this single counter-driven path and rely on the
        // reducer (via shadow ChatState or its Step 5 successor) for any
        // re-ordering protection beyond what mpsc already provides.
        let draft_updater = if let Some(ref d_id) = draft_id {
            let channel: Arc<TerminalChannel> = Arc::clone(&terminal);
            let reply_target = "user".to_string();
            let draft_id_owned = d_id.clone();
            let mut rx = delta_rx;
            let version_counter = Arc::new(DraftVersionCounter::new());
            // Step 5b 双写：把每个 delta 通过 coalescer 投递成 Action::StreamChunkReceived。
            // bounded(2048) action_tx 满时由 coalescer 合并 delta，避免无界增长。
            let coalescer_sender = chat_dispatcher.sender();
            let coalescer_draft_id = d_id.clone();
            // S2-A: capture per-turn mode snapshot. In `Off` and `Both` the
            // legacy `update_draft` still runs (renderer source of truth);
            // in `Redux` / `Pure` it is skipped so the reducer-driven path is
            // the sole UI writer. The coalescer dispatch always runs — the
            // reducer needs every delta for its `StreamState::draft` mirror.
            //
            // T3-3 收官（Pure）：与 Redux 同样关闸；guard 用 matches!(Off|Both)
            // 反向命题保证未来新增 ReduxMode 变体时编译期可见漏写.
            #[cfg(feature = "terminal-tui")]
            let legacy_update_draft_enabled = matches!(redux_mode, ReduxMode::Off | ReduxMode::Both);
            #[cfg(not(feature = "terminal-tui"))]
            let legacy_update_draft_enabled = true;
            Some(tokio::spawn(async move {
                let mut accumulated = String::new();
                let mut coalescer = dispatcher::StreamChunkCoalescer::new(coalescer_sender);
                while let Some(delta) = rx.recv().await {
                    accumulated.push_str(&delta);
                    // Counter still ticks for downstream consumers (UiActor's
                    // inline-redraw protocol uses it). No tracker.accept() —
                    // see comment block above.
                    let version = version_counter.next();
                    // S2-A gating: legacy UI write only in Off/Both; pure
                    // Redux mode lets the reducer + EffectExecutor render.
                    if legacy_update_draft_enabled
                        && let Err(e) = channel.update_draft(&reply_target, &draft_id_owned, &accumulated).await
                    {
                        tracing::debug!("Draft update failed: {e}");
                    }
                    // Step 5b shadow: forward the **incremental** delta into
                    // the reducer via the coalescer. The reducer accumulates
                    // its own `draft.accumulated` mirror — feeding the full
                    // `accumulated` string here would cause double-accumulation.
                    // Backpressure or close are silently tolerated (legacy
                    // path remains the renderer source; shadow path observes).
                    let _ = coalescer.try_send_chunk(coalescer_draft_id.clone(), delta, version);
                }
                // Stream ended — flush pending coalescer state, counter goes
                // out of scope; reducer-side version state is cleared on
                // StreamCompleted/Failed/Cancelled (投递在 chat::run 主循环里完成).
                let _ = coalescer.flush();
            }))
        } else {
            // No draft — consume delta_rx so the sender doesn't block
            let mut rx = delta_rx;
            Some(tokio::spawn(async move { while rx.recv().await.is_some() {} }))
        };

        // Register this turn's cancellation token so the Ctrl+C handler can cancel it.
        //
        // S2-B Step 3: 在 Redux 主路径下 `Action::TurnStarted` 已经把 token 写进
        // `state.control.active_cancel`；reducer + `Effect::CancelToken` 接管单击
        // Ctrl+C 的真取消。legacy `active_cancel` Arc 仍由顶层 Ctrl+C handler
        // (mod.rs 持久 ctrl_c() 任务) 和 TUI 内部 `InterruptTurn` 读取 — 这两条
        // 路径无法直接访问 ChatState，所以 legacy 字段在 Off/Both 模式下必须保留写
        // 以确保兜底。Redux / Pure 模式下 legacy 字段不必再写（reducer 是单一真源）。
        #[cfg(feature = "terminal-tui")]
        let legacy_active_cancel_enabled = matches!(redux_mode, ReduxMode::Off | ReduxMode::Both);
        #[cfg(not(feature = "terminal-tui"))]
        let legacy_active_cancel_enabled = true;
        if legacy_active_cancel_enabled {
            *active_cancel.lock() = Some(cancellation.clone());
        }

        // ── Tool event forwarding (visual feedback in terminal) ──
        //
        // P2-7: in addition to the existing notify_tool_* calls (which feed
        // the legacy UiActor renderer in `channels/terminal.rs`), we also
        // mirror every tool event into a `TuiState` instance behind a
        // `parking_lot::Mutex`. The ratatui renderer in `chat/tui.rs` reads
        // from this mirror; full renderer wiring lands in P2-12.
        let (tool_event_tx, mut tool_event_rx) = mpsc::channel::<ToolCallNotification>(TOOL_EVENT_CHANNEL_CAPACITY);
        let terminal_for_tools = Arc::clone(&terminal);
        // P3-3: every producer in this turn now shares the chat-scoped
        // `chat_mirror` (created once at the top of `run`). The previous
        // per-turn `tui_mirror` instance is gone — keeping a per-turn alias
        // here so downstream code that already says `tui_mirror.lock()` keeps
        // compiling, but the underlying `Arc` is the same one the render
        // task and the input task hold.
        #[cfg(feature = "terminal-tui")]
        let tui_mirror: Arc<parking_lot::Mutex<tui::TuiState>> = Arc::clone(&chat_mirror);
        // P0-5 fix: tool start/finish events now have a single mirror path.
        // The UiActor in `channels/terminal.rs::handle_event_tui` is the sole
        // writer of tool cards into `TuiState` (it sanitises name/args via
        // `sanitize_terminal_output` and calls `notify_redraw()` on the same
        // 1-slot channel as `redraw_tx_for_main`). The previous double-mirror
        // here pushed a second card and could reorder Running/Done with the
        // UiActor path under load — the forwarder now just relays the event
        // to the UiActor and lets that path own the mirror mutation.
        let tool_event_forwarder = tokio::spawn(async move {
            while let Some(notif) = tool_event_rx.recv().await {
                match notif {
                    ToolCallNotification::Started { name, args_summary } => {
                        terminal_for_tools.notify_tool_started(&name, &args_summary).await;
                    }
                    ToolCallNotification::Finished {
                        name,
                        success,
                        duration_ms,
                    } => {
                        terminal_for_tools
                            .notify_tool_finished(&name, success, duration_ms)
                            .await;
                    }
                    ToolCallNotification::Progress {
                        iteration,
                        max_iterations,
                    } => {
                        terminal_for_tools.notify_progress(iteration, max_iterations).await;
                    }
                }
            }
        });
        // Log a trace stat so the mirror is observably wired (also keeps the
        // `tui_mirror` binding from being flagged as unused when the renderer
        // wiring lands in P2-12).
        #[cfg(feature = "terminal-tui")]
        tracing::trace!(
            tracked_tool_cards = tui_mirror.lock().last_tool_result_index().map(|i| i + 1).unwrap_or(0),
            "tui_mirror initialized"
        );

        // ── Policy Pipeline for tool access control ──────────────
        let policy_pipeline = PolicyPipeline::from_config(&config);
        let scope_ctx = ScopeContext {
            policy: &security,
            sender: "user",
            channel: "terminal",
            chat_type: "private",
            chat_id: "terminal:user",
            policy_pipeline: Some(&policy_pipeline),
        };

        // ── Timeout budget ───────────────────────────────────────
        let timeout_budget = {
            let base = config.channels_config.message_timeout_secs.max(TIMEOUT_MIN_BASE_SECS);
            let scale = (config.agent.max_tool_iterations.max(1) as u64).min(TIMEOUT_MAX_SCALE_FACTOR);
            Duration::from_secs(base.saturating_mul(scale))
        };

        // ── Retry loop (context overflow recovery + timeout retry) ──
        //
        // Mirrors the retry strategy in channels/mod.rs process_channel_message:
        //  - Context overflow: compact history, retry up to MAX_CONTEXT_OVERFLOW_RETRIES
        //  - Timeout: sleep 2s, retry once
        let mut context_overflow_retries = 0usize;
        let mut timeout_retries = 0usize;
        let mut history_len_before_tools;

        // S2-A refinement: split the coarse `Failed` variant so the Redux
        // dispatch path can distinguish user-driven cancellation from real
        // errors. The legacy renderer still treats every non-Success as a
        // failure (continue), but the reducer now sees the correct semantic
        // (`StreamCancelled` vs `StreamFailed { err, retryable }`).
        enum TurnOutcome {
            Success(String),
            /// User-initiated cancel (Ctrl+C) or `is_tool_loop_cancelled` from
            /// the inner loop. Reducer side maps to `StreamCancelled`.
            Cancelled,
            /// Genuine failure (timeout / context-overflow exhausted / other
            /// error). Carries error string + retryable hint for the reducer's
            /// `StreamFailed` payload (mirrors `dispatcher::TurnOutcomeKind::Failed`).
            FailedWithError {
                err: String,
                retryable: bool,
            },
        }

        // ── Step 5a-4: Route — Redux driver vs Legacy tool loop ──
        //
        // 路由契约：仅当 PRX_CHAT_REDUX=1 + PRX_CHAT_REDUX_DRIVER=1 +
        // tools_registry 为空时切到 dispatcher driver。生产环境核心工具
        // 总会注册，故路由几乎总命中 LegacyToolLoop —— 这是 5a-4 阶段
        // 刻意保守的"测试/演进闸"，确保零回归。tool 协议迁移在 5a-5。
        #[cfg(feature = "terminal-tui")]
        let turn_route = {
            let mode = ReduxMode::from_env();
            let driver_opt_in = driver_opt_in_from_env();
            // 路由判定中允许测试 env 强制把 tools_registry 视为空（5a-4 PTY 验证用）。
            // Codex P2: 生产环境若误开此 env 会丢失工具能力，因此首次命中时 WARN
            // 提示运维（每条 turn 都 warn 太吵——这里靠 tracing::warn_once 语义自然由
            // 用户 LOG 聚合工具去重，或者运维通过 grep RUNTIME 启动日志识别）。
            let force_empty = force_empty_tools_from_env();
            if force_empty && !tools_registry.is_empty() {
                tracing::warn!(
                    "PRX_CHAT_REDUX_DRIVER_FORCE_EMPTY_TOOLS=1 is a TEST-ONLY backdoor; \
                     production tools will be ignored for routing decisions"
                );
            }
            let tools_empty = tools_registry.is_empty() || force_empty;
            let route = route_turn(mode, driver_opt_in, tools_empty);
            // 可观测性：每轮 turn 记录路由结果（生产排障线索 + Codex 审计要求）。
            tracing::info!(
                redux_mode = ?mode,
                driver_opt_in,
                tools_empty,
                tools_count = tools_registry.len(),
                route = ?route,
                fallback_reason = if matches!(route, TurnRoute::LegacyToolLoop) && driver_opt_in && matches!(mode, ReduxMode::Redux) {
                    if tools_empty { "n/a" } else { "non_empty_tools_registry" }
                } else {
                    "n/a"
                },
                "chat::run turn route decision"
            );
            route
        };
        // 非 TUI feature 下 turn_route 不参与控制流（driver 分支被 cfg 屏蔽），
        // 仅作变量保留以让两条 feature 配置下 chat::run 共享同一路由契约。
        #[cfg(not(feature = "terminal-tui"))]
        let _ = TurnRoute::LegacyToolLoop;

        // ── Redux Driver 切闸路径（Step 5a-4） ─────────────────────
        //
        // 仅在路由命中 ReduxDriver 时进入。dispatch Action::StartLLMTurn →
        // EffectExecutor::execute_real(Effect::StartTurn) → spawn drive_start_turn_stream
        // 流式驱动 → 通过 action_tx 回投 StreamChunkReceived / Completed / Failed /
        // Cancelled → dispatcher task reduce 后 turn_signal.record_and_notify →
        // 此处 await 拿 outcome。
        //
        // 此分支**不调** run_tool_call_loop，旧路径完全不跑：
        //   * 无 hook 双发（旧路径 hooks.emit 不执行；reducer 内 NotifyHook(Error) 独写）
        //   * 无 history 双写（reducer 通过 RecordAssistantTurn 单写）
        //   * round 2 hang 防御：tokio::select! 上 shutdown.cancelled() 兜底
        //
        // 进入条件 `tools_registry.is_empty()` 保证不会丢失工具调用——driver
        // 协议（StreamChunk）天然不承载 tool_calls，5a-5 推进 tool 协议迁移。
        #[cfg(feature = "terminal-tui")]
        if matches!(turn_route, TurnRoute::ReduxDriver)
            && let Some(d_id) = draft_id.clone()
        {
            // 协议：先获取 notified() future，再 dispatch，再 await。
            // 在 dispatch 前消费旧 outcome 残留以确保读到的是本轮的。
            let notify_fut = turn_signal.notified();
            let _ = turn_signal.consume_outcome();

            // S2.5 P1-A: 显式分支处理 dispatch_result（StartLLMTurn 失败必须 fall-through
            // 否则 notify_fut 永挂）；dispatch_or_log 同时埋点 + warn，无需重复 tracing.
            let dispatch_result = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::StartLLMTurn {
                    draft_id: d_id.clone(),
                    history: history.clone(),
                    cancel: cancellation.clone(),
                },
                "chat.start_llm_turn",
            );
            // Codex P1：dispatch 可能 Backpressured / ChannelClosed。任一失败
            // 都意味着 dispatcher task 不会产生 turn outcome，chat::run 必须立即
            // 视为 Failed 并 fall-through 到 cleanup，否则 notify_fut 永远不被 fire。
            if !matches!(dispatch_result, dispatcher::DispatchResult::Sent) {
                tracing::warn!(
                    result = ?dispatch_result,
                    "Redux driver: StartLLMTurn dispatch failed; aborting turn"
                );
                // S2-B Step 3: 同步发 StreamCancelled 让 reducer 清 active_cancel，
                // 旧字段仅在 Off/Both 兜底（与 register 处的守卫对称）。
                if let Some(ref d_id) = draft_id {
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::StreamCancelled { draft_id: d_id.clone() },
                        "chat.stream_cancelled_dispatch_failed",
                    );
                }
                if legacy_active_cancel_enabled {
                    *active_cancel.lock() = None;
                }
                drop(delta_tx);
                drop(tool_event_tx);
                if let Some(handle) = draft_updater {
                    let _ = handle.await;
                }
                let _ = tool_event_forwarder.await;
                if let Some(ref id) = draft_id {
                    let _ = terminal.cancel_draft("user", id).await;
                }
                eprintln!("\nError: redux driver dispatch failed\n");
                continue;
            }

            // shutdown 抢占保护防 round 2 hang。
            tokio::select! {
                () = notify_fut => {}
                () = shutdown.cancelled() => {
                    tracing::debug!("Redux driver: shutdown.cancelled before turn complete");
                }
            }

            let outcome = turn_signal.consume_outcome();

            // Finalize streaming（与 legacy 收尾对齐）：drop senders 让后台任务收口.
            //
            // S2-B Step 3: driver 路径下 reducer 在收到 StreamCompleted/Failed/Cancelled
            // 时已经清掉 `state.control.active_cancel`；legacy Arc 仅在 Off/Both 兜底.
            if legacy_active_cancel_enabled {
                *active_cancel.lock() = None;
            }
            drop(delta_tx);
            drop(tool_event_tx);
            if let Some(handle) = draft_updater {
                let _ = handle.await;
            }
            let _ = tool_event_forwarder.await;

            match outcome {
                Some(dispatcher::TurnOutcomeKind::Completed { final_text }) => {
                    // 1) 把 driver 流式累计的最终文本写回 LLM history（与 legacy 行尾
                    //    `history.push(ChatMessage::assistant(...))` 对齐）。
                    history.push(ChatMessage::assistant(final_text.clone()));
                    // 2) finalize_draft：把文本投递给 terminal channel 让用户可见
                    //    （driver 路径不走 delta_tx → draft_updater 链路，直接最终化）。
                    if let Err(e) = terminal.finalize_draft("user", &d_id, &final_text).await {
                        tracing::warn!(error = %e, "Redux driver: finalize_draft failed");
                    }
                    // driver 路径 RecordAssistantTurn 已由 dispatcher.rs send（fixB B5）
                    let _ = final_text;
                }
                Some(dispatcher::TurnOutcomeKind::Failed { err, retryable: _ }) => {
                    // reducer NotifyHook(Error) 已发；这里不再 hooks.emit 避免双发.
                    if let Some(ref id) = draft_id {
                        let _ = terminal.cancel_draft("user", id).await;
                    }
                    eprintln!("\nError: {err}\n");
                }
                Some(dispatcher::TurnOutcomeKind::Cancelled) | None => {
                    if let Some(ref id) = draft_id {
                        let _ = terminal.cancel_draft("user", id).await;
                    }
                }
            }

            continue;
        }

        let turn_outcome = loop {
            history_len_before_tools = history.len();

            let result = tokio::time::timeout(
                timeout_budget,
                run_tool_call_loop(
                    provider.as_ref(),
                    &mut history,
                    &tools_registry,
                    observer.as_ref(),
                    &hooks,
                    provider_name,
                    model_name,
                    temperature,
                    false,
                    Some(&approval_manager),
                    "terminal",
                    &config.multimodal,
                    config.agent.max_tool_iterations,
                    config.agent.parallel_tools,
                    config.agent.read_only_tool_concurrency_window,
                    config.agent.read_only_tool_timeout_secs,
                    config.agent.priority_scheduling_enabled,
                    config.agent.low_priority_tools.clone(),
                    ToolConcurrencyGovernanceConfig {
                        kill_switch_force_serial: config.agent.concurrency_kill_switch_force_serial,
                        rollout_stage: config.agent.concurrency_rollout_stage.clone(),
                        rollout_sample_percent: config.agent.concurrency_rollout_sample_percent,
                        rollout_channels: config.agent.concurrency_rollout_channels.clone(),
                        auto_rollback_enabled: config.agent.concurrency_auto_rollback_enabled,
                        rollback_timeout_rate_threshold: config.agent.concurrency_rollback_timeout_rate_threshold,
                        rollback_cancel_rate_threshold: config.agent.concurrency_rollback_cancel_rate_threshold,
                        rollback_error_rate_threshold: config.agent.concurrency_rollback_error_rate_threshold,
                    },
                    Some(&config.agent.compaction),
                    Some(cancellation.clone()),
                    Some(delta_tx.clone()),
                    Some(&scope_ctx),
                    Some(tool_event_tx.clone()),
                    Some(&config.tool_tiering),
                    chat_session.mode,
                ),
            )
            .await;

            match result {
                // ── Timeout ───────────────────────────────────────
                Err(_elapsed) => {
                    if timeout_retries < 1 {
                        timeout_retries += 1;
                        tracing::warn!("LLM timeout, retrying (attempt {timeout_retries}/1)");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    // Exhausted timeout retries
                    cancellation.cancel();
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    eprintln!("\nError: operation timed out\n");
                    // Phase E (5a-4): dual_write_guard 守卫防止与 reducer 的 NotifyHook(Error)
                    // 在 Both / Redux 双写期产生双发（reducer 通过 Effect::NotifyHook 已发）。
                    // Off 模式 guard 永远 false → 行为不变（旧路径单发）。
                    if !dual_write_guard.is_active() {
                        hooks
                            .emit(HookEvent::Error, payload_error("chat-turn", "timeout"))
                            .await;
                    }
                    // S2-A: timeout exhausted is a non-retryable hard failure.
                    break TurnOutcome::FailedWithError {
                        err: "timeout".to_string(),
                        retryable: false,
                    };
                }
                // ── Success ───────────────────────────────────────
                Ok(Ok(resp)) => break TurnOutcome::Success(resp),
                // ── Cancelled (Ctrl+C) ────────────────────────────
                Ok(Err(ref e)) if is_tool_loop_cancelled(e) || cancellation.is_cancelled() => {
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    // S2-A: user-driven cancel — distinguished from real
                    // failures so the reducer emits `StreamCancelled` (no
                    // Error hook fan-out) instead of `StreamFailed`.
                    break TurnOutcome::Cancelled;
                }
                // ── Context window overflow → compact + retry ─────
                Ok(Err(ref e)) if is_context_window_overflow_error(e) => {
                    // S2-B Step 4: dispatch `HistoryCompacted` 让 reducer 对
                    // `state.session.history` 应用同样的 compaction 算法（两侧共享
                    // COMPACT_KEEP_MESSAGES/COMPACT_CONTENT_CHARS/COMPACT_TOTAL_CHARS
                    // 三个常量，state.rs 与 mod.rs 同源 → 字节级一致）。
                    // legacy `compact_chat_history(&mut history)` 仍 unconditional 跑，
                    // 因为 `history` 是真实喂给 `run_tool_call_loop` 的 LLM 上下文 Vec —
                    // S2-C 删除 legacy 路径前不能跳过它，否则 Redux 模式下 overflow
                    // 重试会拿同一份未压缩的 history 二次失败。
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::HistoryCompacted {
                            reason: crate::chat::action::CompactReason::ContextOverflow,
                        },
                        "chat.history_compacted_overflow",
                    );
                    compact_chat_history(&mut history);
                    let compacted_chars: usize = history.iter().map(|m| m.content.chars().count()).sum();
                    tracing::warn!(
                        retries = context_overflow_retries,
                        compacted_chars,
                        "Context window overflow, history compacted"
                    );

                    if context_overflow_retries < MAX_CONTEXT_OVERFLOW_RETRIES {
                        context_overflow_retries += 1;
                        continue;
                    }
                    // Exhausted overflow retries
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    eprintln!(
                        "\nError: context window exceeded after {} compaction retries\n",
                        MAX_CONTEXT_OVERFLOW_RETRIES
                    );
                    // Phase E (5a-4): dual_write_guard 守卫见 timeout 分支同理.
                    if !dual_write_guard.is_active() {
                        hooks
                            .emit(
                                HookEvent::Error,
                                payload_error("chat-turn", "context-overflow-exhausted"),
                            )
                            .await;
                    }
                    // S2-A: compaction retries exhausted — non-retryable.
                    break TurnOutcome::FailedWithError {
                        err: "context-overflow-exhausted".to_string(),
                        retryable: false,
                    };
                }
                // ── Other errors ──────────────────────────────────
                Ok(Err(e)) => {
                    if let Some(ref d_id) = draft_id {
                        let _ = terminal.cancel_draft("user", d_id).await;
                    }
                    let err_text = e.to_string();
                    eprintln!("\nError: {err_text}\n");
                    // Phase E (5a-4): dual_write_guard 守卫见 timeout 分支同理.
                    if !dual_write_guard.is_active() {
                        hooks
                            .emit(HookEvent::Error, payload_error("chat-turn", &err_text))
                            .await;
                    }
                    // S2-A: generic provider/loop error — retryable hint is
                    // false (the caller already chose to surface and continue;
                    // retry policy is owned by upstream once tooling lands).
                    break TurnOutcome::FailedWithError {
                        err: err_text,
                        retryable: false,
                    };
                }
            }
        };

        // ── Finalize streaming ────────────────────────────────────
        // Deregister this turn's cancellation token.
        //
        // S2-B Step 3: legacy 路径下 reducer 在 Stream{Completed,Cancelled,Failed}
        // dispatch (下方 1886-1911) 时也清 `state.control.active_cancel`；legacy
        // Arc 仅在 Off/Both 模式兜底（外部 Ctrl+C handler 读这个）。
        if legacy_active_cancel_enabled {
            *active_cancel.lock() = None;
        }

        // Drop our channel senders so background tasks receive channel close
        drop(delta_tx);
        drop(tool_event_tx);
        if let Some(handle) = draft_updater {
            let _ = handle.await;
        }
        let _ = tool_event_forwarder.await;

        // Step 5b 双写：根据 turn 结果投递相应的流式结束 Action。
        // 当 draft_id 存在时 reducer 才能匹配 stream.draft 并清理；否则 no-op.
        //
        // S2-A: split the previous single-pronged "Failed → StreamCancelled"
        // fallback. Order is **critical**: cancellation is detected up the
        // stack via `is_tool_loop_cancelled` / `cancellation.is_cancelled()`
        // and surfaces as `TurnOutcome::Cancelled`. Real errors (timeout /
        // context overflow / provider error) surface as `FailedWithError` and
        // map to `StreamFailed { err, retryable }` so the reducer emits the
        // `NotifyHook(Error) + LogTrace + RequestRedraw` effect chain.
        //
        // T3-3-fixA P0-1: Success 分支的 StreamCompleted 已下移到 RecordAssistantTurn
        // 之后 dispatch，确保 reducer 构造 SaveSession 快照时 session.turns 已含当轮
        // assistant。Cancelled / FailedWithError 不写 assistant turn，dispatch 位置不变。
        match &turn_outcome {
            TurnOutcome::Success(_) => {}
            TurnOutcome::Cancelled => {
                if let Some(ref d_id) = draft_id {
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::StreamCancelled { draft_id: d_id.clone() },
                        "chat.stream_cancelled",
                    );
                }
            }
            TurnOutcome::FailedWithError { err, retryable } => {
                if let Some(ref d_id) = draft_id {
                    let _ = chat_dispatcher.dispatch_or_log(
                        crate::chat::action::Action::StreamFailed {
                            draft_id: d_id.clone(),
                            err: err.clone(),
                            retryable: *retryable,
                        },
                        "chat.stream_failed",
                    );
                }
            }
        }

        // If the turn failed or was cancelled, skip response processing
        let response = match turn_outcome {
            TurnOutcome::Success(resp) => resp,
            TurnOutcome::Cancelled | TurnOutcome::FailedWithError { .. } => continue,
        };

        // ── P2-12: Mirror reasoning content into the TUI as a folded card. ──
        //
        // Reasoning is separated upstream at the provider layer (P0-2) and
        // persisted into the assistant-history JSON via
        // `build_native_assistant_history`. We scan the slice of history
        // produced during this turn for `reasoning_content` fields, aggregate
        // them, and push a single folded `Reasoning` card to the TUI mirror.
        // Empty buffers are skipped by `push_reasoning`. This does NOT touch
        // the visible delta stream — the user-facing assistant text remains
        // the only thing rendered in the streaming draft.
        #[cfg(feature = "terminal-tui")]
        {
            let turn_slice = history.get(history_len_before_tools..).unwrap_or(&[]);
            let aggregated = collect_reasoning_from_history_slice(turn_slice);
            if !aggregated.is_empty() {
                tui_mirror.lock().push_reasoning(&aggregated);
                // The reasoning card is a folded payload appended after the
                // tool sequence but before the assistant draft is committed
                // to scrollback; wake the unified loop so it materialises
                // on the next iteration instead of after a 50 ms poll.
                if let Some(tx) = redraw_tx_for_main.as_ref() {
                    let _ = tx.try_send(());
                }
            }
        }

        increment_recalled_useful_counts(mem.as_ref(), &mem_context.ids).await;

        // ── Sanitize response: strip tool-call XML/JSON artifacts ──
        let response = sanitize_channel_response(&response, &tools_registry);

        // ── Extract tool context summary for LLM awareness on next turn ──
        let tool_summary = extract_tool_context_summary(&history, history_len_before_tools);
        // Always persist the assistant response to history. When tools were
        // invoked, prepend the summary so the LLM retains awareness.
        let history_response = if tool_summary.is_empty() {
            response.clone()
        } else {
            format!("{tool_summary}\n{response}")
        };
        history.push(ChatMessage::assistant(&history_response));

        // S2-B Step 4: dispatch RecordAssistantTurn(history_response) 在与 legacy
        // `history.push(ChatMessage::assistant(...))` 同一点 — reducer 的
        // session.history 与 legacy history 字节级对齐。下方 line 2055 处的
        // 旧 dispatch 用 sanitized_response，与 history.push 内容不同 — S2-B Step 4
        // 起改在此处 dispatch 用 history_response，下方旧 dispatch 删除。
        let _ = chat_dispatcher.dispatch_or_log(
            crate::chat::action::Action::RecordAssistantTurn(history_response.clone()),
            "chat.record_assistant_turn",
        );

        // T3-3-fixA P0-1: StreamCompleted 必须在 RecordAssistantTurn 之后 dispatch，
        // reducer 的 reduce_stream_completed 会 emit Effect::SaveSession(snapshot)，
        // 此时 session.turns 已含当轮 assistant —— 否则 SaveSession 落盘旧快照。
        // final_text 用 response (UI 展示文案)，与上方 history_response (含 tool_summary
        // 前缀供 history 写入) 语义不同：reducer 的 conversation_lines 与 UI 对齐.
        if let Some(ref d_id) = draft_id {
            let _ = chat_dispatcher.dispatch_or_log(
                crate::chat::action::Action::StreamCompleted {
                    draft_id: d_id.clone(),
                    final_text: response.clone(),
                    reasoning: String::new(),
                },
                "chat.stream_completed",
            );
        }

        // ── Extract and display media markers (images, documents, etc.) ──
        let (clean_response, media_items) = extract_outgoing_media(&response);
        for (media_type, path) in &media_items {
            if media_type == "IMAGE" && std::path::Path::new(path).exists() {
                if let Err(e) = terminal_proto::display_image(path) {
                    tracing::debug!("Image display failed: {e}");
                    println!("  [image: {path}]");
                }
            } else {
                println!("  [{media_type}: {path}]");
            }
        }
        // Use the cleaned response (media markers removed) for display
        let display_response = if media_items.is_empty() {
            &response
        } else {
            &clean_response
        };

        // Finalize the streaming draft with the full response.
        //
        // Idempotency contract (P1-4):
        //   When a draft exists, the streamed deltas have already painted the
        //   full response on screen via `update_draft`. `finalize_draft` is
        //   only a structural close (locking in the final text for non-TTY
        //   channels such as Telegram). On failure we therefore MUST NOT
        //   re-send the whole message — that would duplicate output that the
        //   user has already seen via the live stream. We log a warning and
        //   carry on; the assistant turn is already in `history`, so the
        //   conversation state remains consistent.
        //
        //   The "no draft" branch is the genuine first-send path (drafts were
        //   never created, e.g. `send_draft` failed earlier and returned
        //   `None`), so a normal `send` is correct and not a duplicate.
        if let Some(ref d_id) = draft_id {
            if let Err(e) = terminal.finalize_draft("user", d_id, display_response).await {
                if should_resend_on_finalize_failure(true) {
                    let rendered = render_response(display_response);
                    let _ = terminal.send(&SendMessage::new(rendered, "user")).await;
                } else {
                    tracing::warn!(
                        error = %e,
                        "finalize_draft failed; suppressing resend to preserve idempotency (user already saw streamed content)"
                    );
                }
            }
        } else {
            // No draft was created — send as a complete message with highlighting.
            let rendered = render_response(display_response);
            let _ = terminal.send(&SendMessage::new(rendered, "user")).await;
        }

        // ── Record turn in session + persist ───────────────────
        // Sanitize content before persistence (redact secrets, truncate large outputs).
        //
        // S2-B Step 4: RecordUserTurn / RecordAssistantTurn 已经在上面（enriched /
        // history_response 同点）dispatch；这里 legacy `chat_session.add_*_turn` 在
        // `Off` / `Both` / `Redux` 模式下保留，因为 `chat_session` 仍是
        // `save_session(mem, &chat_session)` 的真实持久化源。
        //
        // T3-3-c 收官：**Pure 模式跳过 legacy add_*_turn** —— reducer 的
        // `RecordUserTurn` / `RecordAssistantTurn` + `Effect::SaveSession` 接管
        // 单源持久化，下方 `save_session(...)` 也由 `dual_write_guard` 抑制。
        // 这关闭了 S2-D/E 阶段保留的最后一处双写残留。
        let sanitized_input = sanitize::sanitize_for_persistence(&user_input);
        let sanitized_response = sanitize::sanitize_for_persistence(&response);
        #[cfg(feature = "terminal-tui")]
        let legacy_session_writes_enabled = !ReduxMode::from_env().is_pure();
        #[cfg(not(feature = "terminal-tui"))]
        let legacy_session_writes_enabled = true;
        if legacy_session_writes_enabled {
            chat_session.add_user_turn(&sanitized_input);
            chat_session.add_assistant_turn(&sanitized_response, Vec::new());
        } else {
            tracing::debug!("T3-3-c Pure mode: skip legacy chat_session.add_*_turn (reducer owns persistence)");
        }

        // P0-1 fix: 旧路径在 Both/Redux 模式下受 dual_write_guard 守卫。
        // Redux reducer 的 SaveSession effect 已在 execute_real 中置位 guard，
        // 若 guard 已激活则旧路径跳过 save_session + hooks.emit(TurnComplete)，
        // 防止 hooks/webhook 双触发（hooks/webhook 不幂等，真会双发）。
        // Off 模式下 guard 永远 false，旧路径如常单写。
        // 选 turn-level（而非 effect-level）：整个 turn 期间只要 Redux 执行了
        // SaveSession/NotifyHook 之一，guard 即 active，旧路径的所有后续写都被抑制。
        if !dual_write_guard.is_active() {
            if let Err(e) = save_session(mem.as_ref(), &chat_session).await {
                tracing::warn!("Failed to persist session: {e}");
            }

            observer.record_event(&ObserverEvent::TurnComplete);
            hooks
                .emit(
                    HookEvent::TurnComplete,
                    serde_json::json!({
                        "mode": "chat",
                        "response_chars": response.chars().count(),
                    }),
                )
                .await;
        } else {
            // Guard active: Redux path already handled save_session + hooks.emit.
            // 旧路径跳过，避免双写/双发。
            tracing::debug!(
                "P0-1: dual_write_guard active — legacy path skipping save_session + hooks.emit(TurnComplete)"
            );
            // observer.record_event 仍然调用（observer 只是本地计数，无外部副作用）
            observer.record_event(&ObserverEvent::TurnComplete);
        }
    }

    // ── Graceful teardown: restore terminal state ────────────────
    //
    // On the TUI path (`terminal-tui` feature + TTY + PRX_TUI != "0"), terminal
    // state is owned by `TerminalGuard` (entered above, dropped at end
    // of scope) — the calls below are then redundant but idempotent
    // and harmless. On the legacy reedline / non-TUI path no guard was
    // ever created, so this is the only place that restores terminal
    // state in case reedline or any helper left it dirty. Kept here as
    // belt-and-braces defence; do not remove without also auditing
    // every non-TUI exit path.
    //
    // P3-inline: no `LeaveAlternateScreen` — we never entered it. The
    // inline viewport's content (status / streaming / input / footer)
    // simply stops being redrawn; the host shell takes over on the row
    // immediately below the last permanent message that was pushed via
    // `terminal.insert_before`.
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(std::io::stderr(), crossterm::cursor::Show);

    // Step 5b: dispatcher task graceful shutdown.
    //
    // 1. shutdown.cancel() 让所有 spawn 出去的信号 handler / TUI loop 主动退出，
    //    释放它们持有的 chat_dispatcher sender clone（否则 action_rx 永远不会
    //    自然 close，dispatcher_handle.await 会 hang）。
    // 2. drop(chat_dispatcher) 释放主路径持有的 sender。
    // 3. dispatcher_handle.await 收尾——select! 中 shutdown.cancelled() 分支立即
    //    触发，dispatcher 退出。main.rs:866 的 RUNTIME_SHUTDOWN_TIMEOUT (2s)
    //    仍兜底（不可改）。
    shutdown.cancel();
    drop(chat_dispatcher);
    match dispatcher_handle.await {
        Ok(stats) => tracing::info!(
            actions = stats.actions_seen,
            effects = stats.effects_seen,
            "redux dispatcher shutdown clean"
        ),
        Err(e) => tracing::warn!(error = %e, "redux dispatcher join failed"),
    }

    // T3-3-fixA P0-2: 退出 save_session Pure 守卫.
    //
    // Pure 模式下 chat_session.add_*_turn 被 line 2185 守卫跳过，chat_session.turns
    // 滞后于 reducer 维护的 SessionState。无条件退出 save 会用旧快照覆盖 reducer
    // 已落盘的最新 snapshot。守卫表达式与 line 2185 同形结构保持一致.
    #[cfg(feature = "terminal-tui")]
    let legacy_exit_save_enabled = !ReduxMode::from_env().is_pure();
    #[cfg(not(feature = "terminal-tui"))]
    let legacy_exit_save_enabled = true;
    if legacy_exit_save_enabled {
        // Final session save before exit
        if let Err(e) = save_session(mem.as_ref(), &chat_session).await {
            tracing::warn!("Failed to persist session on exit: {e}");
        }
    } else {
        tracing::debug!("T3-3-fixA P0-2 Pure mode: skip legacy exit save_session (reducer owns persistence)");
    }

    info!("Chat session ended");
    Ok(())
}

// ── Response rendering ───────────────────────────────────────────────────

/// Idempotency policy for the `finalize_draft` failure path.
///
/// Returns `true` if the caller should re-send the full response as a fresh
/// message when `finalize_draft` returns `Err`. Today this is always `false`
/// when a draft was active: streamed deltas have already painted the full
/// text on screen via `update_draft`, so resending would duplicate the
/// assistant turn (this was the P1-4 regression).
///
/// `had_active_draft = true` means a `send_draft` previously succeeded and
/// the user has seen the streamed output. The function is intentionally a
/// pure decision so it can be unit-tested without spinning up a channel.
const fn should_resend_on_finalize_failure(had_active_draft: bool) -> bool {
    // If a draft was active, the user already saw the streamed response —
    // resending would duplicate it. The "no draft" path is handled by the
    // caller directly (it is the genuine first send, not a fallback).
    !had_active_draft
}

/// Apply markdown highlighting to a response (when terminal-tui feature is active).
/// Falls back to plain formatting with newline wrapping.
fn render_response(response: &str) -> String {
    #[cfg(feature = "terminal-tui")]
    {
        format!("\n{}\n", renderer::render_markdown_with_highlighting(response))
    }
    #[cfg(not(feature = "terminal-tui"))]
    {
        format!("\n{response}\n")
    }
}

// ── P3-3: ratatui render task ────────────────────────────────────────────

/// Spawn the blocking `ratatui::Terminal` render task.
///
/// Owns a `Terminal<CrosstermBackend<Stdout>>` for the duration of the chat
/// session and redraws the four-area layout (status / output / input /
/// footer) on demand. Demand is signalled by any producer that mutated the
/// shared `Arc<Mutex<TuiState>>`; the wakeup channel is a `tokio::sync::mpsc`
/// of capacity 1 used as a coalescer (multiple `try_send(())` calls collapse
/// into a single deferred redraw).
///
/// Runs inside `tokio::task::spawn_blocking` because `terminal.draw()`
/// performs synchronous I/O and `mpsc::Receiver::blocking_recv()` blocks the
/// caller. Returning a `JoinHandle` lets the caller observe panics if
/// desired (the chat loop currently fires-and-forgets — terminal restoration
/// is owned by `TerminalGuard::Drop`).
///
/// Lock policy: the render path takes the mirror lock for as briefly as
/// possible (the borrow is dropped before the next iteration parks).
/// Producers hold the same lock only across short, non-blocking mutations,
/// so the renderer never starves.
/// Spawn the unified ratatui TUI loop on a dedicated blocking thread.
///
/// **Single-thread architecture (P3 rearch).** The previous design split
/// rendering, input reading, and a periodic heartbeat across three separate
/// `spawn_blocking` tasks, each of which held its own raw `std::io::stdout()`
/// handle. crossterm's internal ANSI queries (DA1/DSR, used during key
/// dispatch and bracketed-paste decoding) wrote bytes to the same stdout
/// that ratatui's buffer flush was writing to — so characters made it into
/// ratatui's internal buffer but were partially overwritten on the wire,
/// producing the historic "I typed but nothing appears" bug.
///
/// This single loop is what every reference implementation does
/// (`ratatui/examples/user_input.rs`, OpenAI codex-rs, atuin, yazi, helix,
/// zellij): **one thread owns the Terminal + stdout**, reads events
/// directly, and redraws between events. There is no second stdout writer
/// and no producer/consumer split that can starve the renderer.
///
/// Wakeup sources:
///   * Each `crossterm::event::poll(50 ms)` returns either a real event
///     (keys / resize / paste / focus / mouse) or a timeout — and on every
///     iteration we redraw, so an in-flight LLM stream pushing deltas into
///     `mirror` shows up within ~50 ms even with no keypress.
///   * `redraw_rx` lets the UiActor wake us immediately when a streaming
///     event arrives; we drain it (coalesce) and let the next loop top
///     redraw.
///   * `shutdown` cancels the loop on `Ctrl+D` (empty buffer), double
///     `Ctrl+C`, or SIGTERM.
///
/// The function intentionally does NOT touch raw-mode / alt-screen state —
/// that is owned by [`TerminalGuard`] in `run()`. If `Terminal::new` fails
/// the task logs and exits; the guard's Drop still restores the terminal.
#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
fn spawn_tui_unified_loop(
    input_tx: mpsc::Sender<crate::channels::traits::ChannelMessage>,
    mirror: Arc<parking_lot::Mutex<tui::TuiState>>,
    redraw_rx: mpsc::Receiver<()>,
    redraw_tx: mpsc::Sender<()>,
    shutdown: CancellationToken,
    last_ctrlc_ms: Arc<AtomicU64>,
    active_cancel: Arc<parking_lot::Mutex<Option<CancellationToken>>>,
    chat_dispatcher: dispatcher::ChatDispatcher,
) {
    tokio::task::spawn_blocking(move || {
        let result = run_tui_unified_loop(
            input_tx,
            mirror,
            redraw_rx,
            redraw_tx,
            &shutdown,
            last_ctrlc_ms,
            active_cancel,
            &chat_dispatcher,
        );
        if let Err(e) = result {
            tracing::error!("TUI unified loop error: {e}");
        }
    });
}

/// Inner body of [`spawn_tui_unified_loop`].
///
/// **P3-inline architecture.** ratatui owns only a fixed-height inline
/// viewport at the bottom of the terminal — see [`tui::render_bottom_chrome`].
/// Permanent conversation history is pushed up into the host terminal's
/// main scrollback at the top of each loop iteration via
/// `terminal.insert_before`. Once a [`tui::ConversationLine`] has been
/// pushed it lives in the user's normal terminal scrollback, scrolled by
/// mouse wheel / Shift+PgUp / terminal search like any other shell
/// output — there is no app-level scrollbar or scroll state to manage.
#[cfg(feature = "terminal-tui")]
#[allow(clippy::too_many_arguments)]
fn run_tui_unified_loop(
    input_tx: mpsc::Sender<crate::channels::traits::ChannelMessage>,
    mirror: Arc<parking_lot::Mutex<tui::TuiState>>,
    mut redraw_rx: mpsc::Receiver<()>,
    redraw_tx: mpsc::Sender<()>,
    shutdown: &CancellationToken,
    last_ctrlc_ms: Arc<AtomicU64>,
    active_cancel: Arc<parking_lot::Mutex<Option<CancellationToken>>>,
    chat_dispatcher: &dispatcher::ChatDispatcher,
) -> Result<()> {
    use crate::channels::traits::ChannelMessage;
    use crate::chat::action::Action;
    use crate::chat::state::{ChatState, Effect};
    use crossterm::event::{Event, KeyEventKind};
    use ratatui::{TerminalOptions, Viewport};

    // Step 2: 双写灰度 — `PRX_CHAT_REDUX` 控制 reducer 路径是否生效
    //   未设 / "0"  → 旧路径单写（默认；reducer 不构造）
    //   "both"      → 双写（旧路径 + reducer，比对效果用于回归排查）
    //   "1"         → 新路径影响关键控制流（仅 Quit / Ctrl+D 空 buffer 走 reducer；
    //                   InterruptTurn / cancel 仍走旧路径，Step 4 才完整迁移）
    let redux_mode = ReduxMode::from_env();
    // shadow ChatState 仅在 redux_mode != Off 时构造；占位用 dummy provider/model
    let mut shadow: Option<ChatState> = if matches!(redux_mode, ReduxMode::Off) {
        None
    } else {
        let (provider_name, model_name) = {
            let guard = mirror.lock();
            (guard.provider.clone(), guard.model.clone())
        };
        Some(ChatState::new(
            Arc::from(provider_name.as_str()),
            Arc::from(model_name.as_str()),
            shutdown.clone(),
        ))
    };
    if !matches!(redux_mode, ReduxMode::Off) {
        tracing::info!(mode = ?redux_mode, "PRX_CHAT_REDUX active — Step 2 双写模式");
    }

    let stdout = std::io::stdout();
    let backend = ratatui::backend::CrosstermBackend::new(stdout);

    // Inline viewport height is fixed at creation time — ratatui does not
    // support dynamically resizing an `Inline` viewport (see ratatui
    // issue #984; `terminal.resize(Rect)` performs a full clear + viewport
    // recompute and was the cause of the "blank chrome on entry" bug).
    // We allocate the maximum possible chrome height up front and let
    // `render_bottom_chrome` align the actual chrome to the bottom of the
    // reserved area when the dynamic height is smaller.
    let initial_height = tui::BOTTOM_CHROME_MAX_HEIGHT;
    let mut terminal = ratatui::Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(initial_height),
        },
    )
    .map_err(|e| anyhow::anyhow!("ratatui Terminal::with_options failed: {e}"))?;

    // Materialise the inline viewport immediately so the chrome (status
    // bar + input box + footer) is visible the moment the session opens,
    // rather than only after the first event-loop iteration draws.
    terminal
        .draw(|f| tui::render_bottom_chrome(f, &mirror.lock()))
        .map_err(|e| anyhow::anyhow!("initial TUI draw failed: {e}"))?;

    // Number of `conversation_lines` already flushed to the host
    // scrollback via `insert_before`. New entries appear at indices
    // `>= last_pushed_idx` and are pushed on the next loop iteration.
    let mut last_pushed_idx: usize = 0;

    // 50 ms event poll → ~20 fps idle redraw cap. Streaming wakes via
    // `redraw_rx` so this is just a floor, not an upper bound.
    let poll = Duration::from_millis(50);

    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }

        // ── 1. Flush newly-finalised conversation lines to scrollback ──
        // We take the mirror lock briefly to snapshot the pending range
        // and the ASCII fallback flag, then release it BEFORE calling
        // `insert_before` (which performs blocking I/O). This avoids
        // holding the lock across stdout writes — producers can keep
        // pushing into `conversation_lines` while we drain.
        let (pending, ascii_fallback) = {
            let guard = mirror.lock();
            let slice: Vec<tui::ConversationLine> = guard
                .conversation_lines
                .get(last_pushed_idx..)
                .map(<[tui::ConversationLine]>::to_vec)
                .unwrap_or_default();
            (slice, guard.ascii_fallback)
        };
        let pending_count = pending.len();
        let term_width = terminal.size().map(|s| s.width).unwrap_or(80).max(1);
        for line in &pending {
            let height = tui::estimate_message_height(term_width, line, ascii_fallback);
            // `insert_before` is a no-op for `Viewport::Fullscreen` /
            // `Fixed` — safe to call with any height; ratatui scrolls
            // the host terminal up as needed.
            let render_line = line.clone();
            if let Err(e) = terminal.insert_before(height, move |buf| {
                tui::render_message_for_insert(buf, &render_line, ascii_fallback);
            }) {
                tracing::warn!(error = %e, "insert_before failed; skipping line");
            }
        }
        last_pushed_idx = last_pushed_idx.saturating_add(pending_count);

        // ── 2. Drain coalesced redraw wakeups, then redraw the chrome ─
        // Inline-viewport height is fixed at construction time
        // (`BOTTOM_CHROME_MAX_HEIGHT`); calling `terminal.resize` here
        // would trigger a full clear every iteration (ratatui issue
        // #984), which is the bug that this rewrite removes.
        // `render_bottom_chrome` aligns the actual chrome to the bottom
        // of the reserved frame area, so unused rows above stay blank
        // without disturbing scrollback.
        while redraw_rx.try_recv().is_ok() {}
        if let Err(e) = terminal.draw(|f| tui::render_bottom_chrome(f, &mirror.lock())) {
            tracing::warn!(error = %e, "TUI draw failed");
        }

        // ── 3. Wait for the next input event, with a 50 ms floor ──────
        if !crossterm::event::poll(poll)? {
            continue;
        }
        let ev = crossterm::event::read()?;
        // [DIAG] log every raw crossterm event so we can observe what the
        // terminal actually delivers (Chinese IME, paste, resize, etc.).
        match &ev {
            crossterm::event::Event::Key(k) => {
                tracing::info!(
                    event_type = "Key",
                    code = ?k.code,
                    modifiers = ?k.modifiers,
                    kind = ?k.kind,
                    "tui_input_event"
                );
            }
            crossterm::event::Event::Paste(s) => {
                tracing::info!(
                    event_type = "Paste",
                    chars_count = s.chars().count(),
                    bytes_count = s.len(),
                    first_8_chars = %s.chars().take(8).collect::<String>(),
                    "tui_input_event"
                );
            }
            crossterm::event::Event::Resize(w, h) => {
                tracing::info!(event_type = "Resize", w, h, "tui_input_event");
            }
            other => {
                tracing::info!(event_type = "Other", debug = ?other, "tui_input_event");
            }
        }
        match ev {
            Event::Key(key) => {
                // Skip key-release events: terminals with
                // KeyboardEnhancement flags (Kitty et al.) fire both Press
                // and Release for one physical keystroke. Only Press /
                // Repeat are authoritative input.
                if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    continue;
                }
                // Step 2 双写: 如果 redux 模式启用，并行 reduce 到 shadow state。
                // 旧路径仍执行（dispatch_global_key）；shadow 输出的 Effect 仅在
                // Redux mode 下参与控制流（Quit）。Both mode 仅记录用于对账。
                let shadow_effects: Vec<Effect> = shadow
                    .as_mut()
                    .map_or_else(Vec::new, |state| state.reduce(Action::KeyPressed(key)));
                let shadow_wants_quit = shadow_effects.iter().any(|e| matches!(e, Effect::Quit));

                let dispatch = tui::dispatch_global_key(key, &mut mirror.lock());
                // C1 fix: any consumed keystroke may have mutated visible
                // state — typing in the input box, Tab folding a tool card,
                // Ctrl+R folding a reasoning card, Esc clearing the buffer,
                // history navigation. Nudge the loop so the change paints
                // on the next iteration rather than waiting for the next
                // crossterm event (worst case 50 ms idle poll). cap=1 +
                // try_send coalesces, so this is cheap on key floods.
                let _ = redraw_tx.try_send(());
                if matches!(redux_mode, ReduxMode::Both) {
                    log_redux_key_diff(&dispatch, &shadow_effects);
                }
                match dispatch {
                    tui::KeyDispatch::Submitted(text) => {
                        let trimmed = text.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let timestamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let msg = ChannelMessage {
                            id: uuid::Uuid::new_v4().to_string(),
                            sender: "user".to_string(),
                            reply_target: "user".to_string(),
                            content: trimmed,
                            channel: "terminal".to_string(),
                            timestamp,
                            thread_ts: None,
                            mentioned_uuids: vec![],
                        };
                        if input_tx.blocking_send(msg).is_err() {
                            // Receiver dropped — chat::run is tearing down.
                            return Ok(());
                        }
                    }
                    tui::KeyDispatch::Exit => {
                        // Ctrl+D on empty buffer → graceful shutdown.
                        shutdown.cancel();
                        return Ok(());
                    }
                    tui::KeyDispatch::InterruptTurn => {
                        // Raw mode swallows kernel-delivered SIGINT, so we
                        // replicate the persistent ctrl_c() handler's
                        // double-press semantics directly:
                        //   * Two presses within DOUBLE_CTRLC_WINDOW_MS → exit.
                        //   * Otherwise cancel the in-flight turn (if any).
                        let now = now_ms();
                        let prev = last_ctrlc_ms.swap(now, Ordering::Relaxed);
                        if now.saturating_sub(prev) < DOUBLE_CTRLC_WINDOW_MS {
                            // S2-B Step 3: 双击 — 同时 dispatch ShutdownRequested
                            // 让 reducer 真发 CancelToken/Quit (Off/Both 模式仍 fallback
                            // 到 shutdown.cancel()).
                            let _ = chat_dispatcher.dispatch_or_log(
                                crate::chat::action::Action::ShutdownRequested,
                                "chat.shutdown_tui_double_ctrlc",
                            );
                            shutdown.cancel();
                            return Ok(());
                        }
                        // S2-B Step 3: 单击 Ctrl+C — 优先走 reducer 路径
                        // (Action::CancelRequested → Effect::CancelToken → 真 cancel)；
                        // legacy token.cancel() 在 Off/Both 模式兜底，避免漏取消。
                        // Pure / Redux 模式由 reducer 单源负责取消（已 dispatch 到上）。
                        let _ = chat_dispatcher.dispatch_or_log(
                            crate::chat::action::Action::CancelRequested,
                            "chat.cancel_tui_single_ctrlc",
                        );
                        if matches!(redux_mode, ReduxMode::Off | ReduxMode::Both)
                            && let Some(token) = active_cancel.lock().as_ref()
                        {
                            token.cancel();
                        }
                    }
                    tui::KeyDispatch::Cancelled | tui::KeyDispatch::Consumed => {}
                }
                // Redux / Pure mode: shadow 的 Effect::Quit 也能触发退出（用于灰度验证
                // 新路径的 Ctrl+D 空 buffer / 双 Ctrl+C 语义）。Both 模式仅记录差异。
                if matches!(redux_mode, ReduxMode::Redux | ReduxMode::Pure) && shadow_wants_quit {
                    tracing::info!(mode = ?redux_mode, "redux: shadow requested Quit; shutting down");
                    shutdown.cancel();
                    return Ok(());
                }
            }
            Event::Paste(text) => {
                // P3 rearch: bracketed-paste mode (enabled in
                // `TerminalGuard::enter`) is what makes CJK IME input
                // *and* multi-line clipboard paste actually work. Without
                // it, IME commit strings are shredded into per-byte
                // KeyEvents with random modifier bits that
                // `dispatch_global_key` filters out.
                if let Some(state) = shadow.as_mut() {
                    let _ = state.reduce(Action::PasteReceived(text.clone()));
                }
                mirror.lock().input.paste(&text);
                // Paste mutates `input.lines` directly so the chrome must
                // repaint; without this kick the next redraw is gated on
                // the 50 ms poll.
                let _ = redraw_tx.try_send(());
            }
            Event::Resize(w, h) => {
                if let Some(state) = shadow.as_mut() {
                    let _ = state.reduce(Action::TerminalResized { w, h });
                }
                // crossterm forwards the new size to ratatui automatically
                // on the next `draw()` call; we just nudge the loop so the
                // redraw happens immediately rather than waiting up to
                // 50 ms for the next poll. Especially relevant when the
                // user drags a tmux/screen split and expects the chrome
                // (status bar, input box) to reflow on the spot.
                let _ = redraw_tx.try_send(());
            }
            _ => {
                // Focus / mouse / other events — ignore for now.
            }
        }
    }
}

// ── Session persistence helpers ──────────────────────────────────────────

/// Save a session to the Memory backend.
async fn save_session(mem: &dyn Memory, session: &session::ChatSession) -> Result<()> {
    let json = session.to_json().map_err(|e| anyhow::anyhow!("serialize: {e}"))?;
    mem.store(&session.memory_key(), &json, MemoryCategory::Conversation, None)
        .await
        .map_err(|e| anyhow::anyhow!("store: {e}"))?;
    Ok(())
}

/// Load a session by ID (exact key lookup, not similarity search).
async fn load_session_by_id(mem: &dyn Memory, id: &str) -> Option<session::ChatSession> {
    let key = format!("{}:{}", session::SESSION_MEMORY_PREFIX, id);
    let entry = mem.get(&key).await.ok()??;
    session::ChatSession::from_json(&entry.content).ok()
}

/// Load the most recent session.
async fn load_latest_session(mem: &dyn Memory) -> Option<session::ChatSession> {
    let entries = mem.list(Some(&MemoryCategory::Conversation), None).await.ok()?;
    // Find entries with the session prefix, parse and sort by updated_at
    let mut sessions: Vec<session::ChatSession> = entries
        .iter()
        .filter(|e| e.key.starts_with(session::SESSION_MEMORY_PREFIX))
        .filter_map(|e| session::ChatSession::from_json(&e.content).ok())
        .collect();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions.into_iter().next()
}

/// List all saved sessions.
async fn list_saved_sessions(mem: &dyn Memory) -> Result<()> {
    let entries = mem
        .list(Some(&MemoryCategory::Conversation), None)
        .await
        .unwrap_or_default();
    let mut sessions: Vec<session::ChatSession> = entries
        .iter()
        .filter(|e| e.key.starts_with(session::SESSION_MEMORY_PREFIX))
        .filter_map(|e| session::ChatSession::from_json(&e.content).ok())
        .collect();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    if sessions.is_empty() {
        println!("No saved sessions.");
        return Ok(());
    }

    println!("Saved sessions:\n");
    for s in &sessions {
        let title = if s.title.is_empty() { "(untitled)" } else { &s.title };
        println!(
            "  {} | {} | {} turns | {}",
            &s.id[..8.min(s.id.len())],
            title,
            s.turn_count(),
            s.updated_at.format("%Y-%m-%d %H:%M")
        );
    }
    println!("\nResume with: prx chat --session <ID>");
    Ok(())
}

#[cfg(test)]
mod finalize_draft_fallback_tests {
    //! Tests for the P1-4 idempotency contract: when `finalize_draft` fails
    //! for an active draft, the chat loop must NOT re-send the full response
    //! (the streamed deltas already delivered it). The "no draft" path is a
    //! genuine first-send and is still allowed to call `send`.
    use super::*;
    use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Pure decision test: with an active draft, finalize failure must NOT trigger
    /// a resend (would duplicate what the user already saw via stream).
    #[test]
    fn finalize_failure_with_active_draft_does_not_resend() {
        assert!(
            !should_resend_on_finalize_failure(true),
            "active-draft finalize failure must be idempotent (no resend)"
        );
    }

    /// Pure decision test: with no active draft, the caller still uses `send`
    /// directly — that path is the first send, not a fallback resend.
    /// `should_resend_on_finalize_failure(false)` returning `true` simply
    /// documents that the "no draft" branch's normal send is allowed.
    #[test]
    fn no_active_draft_path_allows_send() {
        assert!(
            should_resend_on_finalize_failure(false),
            "no-draft path must allow the normal send (this is a first send, not a duplicate)"
        );
    }

    /// Mock channel that lets us script `finalize_draft` to fail and counts
    /// every method call so we can assert no fallback resend occurs.
    struct MockChannel {
        finalize_should_fail: bool,
        send_calls: AtomicUsize,
        finalize_calls: AtomicUsize,
    }

    impl MockChannel {
        fn new(finalize_should_fail: bool) -> Self {
            Self {
                finalize_should_fail,
                send_calls: AtomicUsize::new(0),
                finalize_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl Channel for MockChannel {
        fn name(&self) -> &str {
            "mock"
        }

        async fn send(&self, _message: &SendMessage) -> anyhow::Result<()> {
            self.send_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn listen(&self, _tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
            Ok(())
        }

        fn supports_draft_updates(&self) -> bool {
            true
        }

        async fn send_draft(&self, _message: &SendMessage) -> anyhow::Result<Option<String>> {
            Ok(Some("draft-1".to_string()))
        }

        async fn finalize_draft(&self, _recipient: &str, _message_id: &str, _text: &str) -> anyhow::Result<()> {
            self.finalize_calls.fetch_add(1, Ordering::SeqCst);
            if self.finalize_should_fail {
                Err(anyhow::anyhow!("simulated finalize failure"))
            } else {
                Ok(())
            }
        }
    }

    /// Replicates the production fallback control flow against a mock channel.
    /// On finalize failure with an active draft, `send` must NOT be invoked.
    async fn run_finalize_path(channel: Arc<MockChannel>, draft_id: Option<String>, display_response: &str) {
        if let Some(ref d_id) = draft_id {
            if let Err(e) = channel.finalize_draft("user", d_id, display_response).await {
                if should_resend_on_finalize_failure(true) {
                    let _ = channel.send(&SendMessage::new(display_response, "user")).await;
                } else {
                    tracing::warn!(error = %e, "finalize_draft failed; suppressing resend");
                }
            }
        } else {
            // First-send path (no draft was ever created)
            let _ = channel.send(&SendMessage::new(display_response, "user")).await;
        }
    }

    #[tokio::test]
    async fn finalize_failure_with_active_draft_suppresses_send() {
        let ch = Arc::new(MockChannel::new(true));
        run_finalize_path(Arc::clone(&ch), Some("draft-1".to_string()), "hello world").await;

        assert_eq!(ch.finalize_calls.load(Ordering::SeqCst), 1, "finalize must run once");
        assert_eq!(
            ch.send_calls.load(Ordering::SeqCst),
            0,
            "finalize failure must NOT trigger a resend when a draft was active"
        );
    }

    #[tokio::test]
    async fn finalize_success_does_not_resend() {
        let ch = Arc::new(MockChannel::new(false));
        run_finalize_path(Arc::clone(&ch), Some("draft-1".to_string()), "hello world").await;

        assert_eq!(ch.finalize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            ch.send_calls.load(Ordering::SeqCst),
            0,
            "successful finalize must not produce any extra send"
        );
    }

    #[tokio::test]
    async fn no_draft_path_sends_once() {
        let ch = Arc::new(MockChannel::new(false));
        run_finalize_path(Arc::clone(&ch), None, "hello world").await;

        assert_eq!(
            ch.finalize_calls.load(Ordering::SeqCst),
            0,
            "no-draft path must not call finalize"
        );
        assert_eq!(
            ch.send_calls.load(Ordering::SeqCst),
            1,
            "no-draft path must send the response exactly once"
        );
    }

    /// Regression guard: two consecutive turns where finalize fails on both
    /// must produce zero extra `send` calls (the previous buggy behavior
    /// resulted in 2 duplicate messages).
    #[tokio::test]
    async fn repeated_finalize_failures_never_resend() {
        let ch = Arc::new(MockChannel::new(true));
        run_finalize_path(Arc::clone(&ch), Some("draft-1".to_string()), "turn 1").await;
        run_finalize_path(Arc::clone(&ch), Some("draft-2".to_string()), "turn 2").await;

        assert_eq!(ch.finalize_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            ch.send_calls.load(Ordering::SeqCst),
            0,
            "repeated finalize failures must remain idempotent (no resends)"
        );
    }
}

#[cfg(all(test, feature = "terminal-tui"))]
mod reasoning_extraction_tests {
    //! Tests for `collect_reasoning_from_history_slice` — the P2-12 helper that
    //! pulls reasoning content from the agent loop's history JSON and feeds it
    //! to the TUI's foldable reasoning card.
    use super::*;

    fn assistant_json(content: Option<&str>, reasoning: Option<&str>) -> ChatMessage {
        let mut obj = serde_json::json!({"content": content, "tool_calls": []});
        if let Some(rc) = reasoning {
            if let Some(map) = obj.as_object_mut() {
                map.insert(
                    "reasoning_content".to_string(),
                    serde_json::Value::String(rc.to_string()),
                );
            }
        }
        ChatMessage::assistant(obj.to_string())
    }

    #[test]
    fn empty_slice_returns_empty_string() {
        assert_eq!(collect_reasoning_from_history_slice(&[]), "");
    }

    #[test]
    fn plain_assistant_text_is_skipped() {
        let history = vec![ChatMessage::assistant("Just a plain answer.")];
        assert_eq!(collect_reasoning_from_history_slice(&history), "");
    }

    #[test]
    fn extracts_single_reasoning_block() {
        let history = vec![assistant_json(Some("ok"), Some("Step 1: think.\nStep 2: act."))];
        let agg = collect_reasoning_from_history_slice(&history);
        assert_eq!(agg, "Step 1: think.\nStep 2: act.");
    }

    #[test]
    fn aggregates_multiple_reasoning_blocks_with_blank_line_separator() {
        let history = vec![
            assistant_json(Some("a"), Some("first thought")),
            ChatMessage::user("user follow-up"),
            assistant_json(Some("b"), Some("second thought")),
        ];
        let agg = collect_reasoning_from_history_slice(&history);
        assert_eq!(agg, "first thought\n\nsecond thought");
    }

    #[test]
    fn whitespace_only_reasoning_dropped() {
        let history = vec![
            assistant_json(Some("a"), Some("   \n\t  ")),
            assistant_json(Some("b"), Some("real reasoning")),
        ];
        let agg = collect_reasoning_from_history_slice(&history);
        assert_eq!(agg, "real reasoning");
    }

    #[test]
    fn malformed_json_is_safely_skipped() {
        // Pre-filter passes (contains "reasoning_content") but JSON parse fails.
        let history = vec![ChatMessage::assistant(
            "broken json with reasoning_content but not valid".to_string(),
        )];
        // Must not panic and must not surface anything.
        assert_eq!(collect_reasoning_from_history_slice(&history), "");
    }

    #[test]
    fn non_assistant_role_ignored_even_with_reasoning_content() {
        // A user message that happens to contain the literal "reasoning_content"
        // must never leak into the reasoning card.
        let history = vec![ChatMessage::user(
            "{\"reasoning_content\":\"shouldn't appear\"}".to_string(),
        )];
        assert_eq!(collect_reasoning_from_history_slice(&history), "");
    }
}

#[cfg(test)]
mod terminal_guard_tests {
    //! Tests for P3-2: `TerminalGuard` RAII + strengthened panic hook.
    //!
    //! These tests intentionally do NOT call `TerminalGuard::enter()` because
    //! the test harness is not connected to a real TTY — `enable_raw_mode`
    //! would fail on most CI runners. Instead we exercise:
    //!
    //!   * `leave()` idempotency on a guard constructed in the "inactive"
    //!     state (no crossterm calls issued).
    //!   * Concurrent `leave()` from multiple threads (Drop + manual).
    //!   * `restore_terminal_state()` does not panic when invoked outside
    //!     raw mode (the panic-hook fast path).
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Build a `TerminalGuard` in the inactive state (no real terminal
    /// mutation), suitable for unit-testing the bookkeeping.
    fn inactive_guard() -> TerminalGuard {
        TerminalGuard {
            raw_mode_active: AtomicBool::new(false),
            alt_screen_active: AtomicBool::new(false),
        }
    }

    /// Build a "fake-active" guard whose flags are set but no real terminal
    /// state was touched. `leave()` will issue crossterm calls but they
    /// no-op safely on a non-TTY test harness (raw mode was never on, so
    /// `disable_raw_mode` is a cheap kernel call returning Ok or Err — both
    /// are swallowed).
    fn fake_active_guard() -> TerminalGuard {
        TerminalGuard {
            raw_mode_active: AtomicBool::new(true),
            alt_screen_active: AtomicBool::new(true),
        }
    }

    #[test]
    fn leave_is_idempotent_on_inactive_guard() {
        let guard = inactive_guard();
        // Multiple calls must not panic and must not flip the flags.
        guard.leave();
        guard.leave();
        guard.leave();
        assert!(!guard.raw_mode_active.load(Ordering::Acquire));
        assert!(!guard.alt_screen_active.load(Ordering::Acquire));
    }

    #[test]
    fn leave_flips_flags_exactly_once() {
        let guard = fake_active_guard();
        assert!(guard.raw_mode_active.load(Ordering::Acquire));
        assert!(guard.alt_screen_active.load(Ordering::Acquire));
        guard.leave();
        assert!(!guard.raw_mode_active.load(Ordering::Acquire));
        assert!(!guard.alt_screen_active.load(Ordering::Acquire));
        // Second leave is a no-op (CAS fails, no crossterm calls).
        guard.leave();
        assert!(!guard.raw_mode_active.load(Ordering::Acquire));
        assert!(!guard.alt_screen_active.load(Ordering::Acquire));
    }

    #[test]
    fn drop_after_manual_leave_is_safe() {
        // Drop must not double-restore — the AtomicBool CAS in leave()
        // ensures the cleanup runs at most once across leave() + drop().
        let guard = fake_active_guard();
        guard.leave();
        // Implicit drop here — must be a no-op (no panic, no extra calls).
    }

    #[test]
    fn concurrent_leave_from_multiple_threads_is_safe() {
        // Simulate two threads racing to clean up the same guard
        // (e.g. manual `leave()` on the main thread + Drop on a panicking
        // background thread). Only one should win the CAS for each flag.
        let guard = Arc::new(fake_active_guard());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let g = Arc::clone(&guard);
            handles.push(std::thread::spawn(move || {
                g.leave();
            }));
        }
        for h in handles {
            h.join().expect("test: worker thread should not panic");
        }
        // After the race both flags must be cleared exactly once.
        assert!(!guard.raw_mode_active.load(Ordering::Acquire));
        assert!(!guard.alt_screen_active.load(Ordering::Acquire));
    }

    #[test]
    fn restore_terminal_state_is_safe_outside_raw_mode() {
        // The panic hook calls this from arbitrary terminal states. It must
        // never panic, even when raw mode was never enabled and we are not
        // inside an alternate screen.
        restore_terminal_state();
        // Calling twice in a row must also be safe (idempotent at the
        // crossterm level — both calls swallow errors).
        restore_terminal_state();
    }

    /// P3-6: the Resize branch in `run_tui_input_loop` only does
    /// `redraw_tx.try_send(()).ok();`. This test pins the coalescing
    /// contract of the cap=1 channel: multiple sends in a row must not
    /// block, they collapse into a single pending wakeup, and the receive
    /// side observes exactly one redraw signal until the next try_send
    /// after the recv. If this contract ever changes, the Resize handler
    /// would silently start blocking the input loop — the test fails
    /// loudly instead.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn resize_redraw_signal_coalesces() {
        use tokio::sync::mpsc;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test: runtime builds");
        rt.block_on(async {
            let (tx, mut rx) = mpsc::channel::<()>(1);
            // Burst of resize-equivalent signals: first one fills the
            // buffer, the rest must fail with Full (non-blocking, no
            // panic). This mirrors a user dragging the window border.
            for _ in 0..16 {
                let _ = tx.try_send(());
            }
            // Exactly one wakeup observable.
            rx.recv().await.expect("test: receives first wakeup");
            // Channel must be drained now.
            assert!(rx.try_recv().is_err(), "expected coalescing to drain to one signal");
            // After drain, the channel must accept a fresh send again.
            tx.try_send(()).expect("test: send after drain succeeds");
            rx.recv().await.expect("test: receives second wakeup");
        });
    }

    #[test]
    fn install_chat_panic_hook_is_idempotent() {
        // Second + later calls must be no-ops (OnceLock-guarded). This test
        // verifies the function does not panic when called repeatedly; the
        // OnceLock state is process-wide so we cannot meaningfully assert
        // which call performed the install — but the absence of panics +
        // unbounded hook nesting is the contract.
        install_chat_panic_hook();
        install_chat_panic_hook();
        install_chat_panic_hook();
    }
}

#[cfg(test)]
mod tracing_redirect_tests {
    //! P3-1: tests for `setup_chat_tracing_to_file_in` and `TracingChatGuard`.
    //!
    //! The reload handle (`crate::CHAT_TRACING_RELOAD`) is a process-wide
    //! `OnceLock` that `main()` initializes only for the `chat` subcommand,
    //! so under `cargo test` it's empty. Happy-path tests cope with both
    //! conditions: a successful redirect yields a guard, an absent reload
    //! handle yields a clean `Err` (and no panic). What we strictly assert
    //! is the I/O contract — directory creation, append-mode open,
    //! non-panicking failure modes — which is what governs whether
    //! `chat::run` can rely on this in production.
    use super::*;

    fn unique_tmpdir(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        base.join(format!("prx-p3-1-{tag}-{pid}-{nanos}"))
    }

    #[test]
    fn setup_creates_directory_and_file() {
        let dir = unique_tmpdir("create");
        let log = dir.join("chat.log");
        assert!(!dir.exists(), "precondition: tmp dir must not exist");
        let result = setup_chat_tracing_to_file_in(&dir);
        // Directory + file must be created regardless of whether the global
        // reload handle is wired up (we open the file *before* reloading).
        assert!(dir.is_dir(), "chat log dir should exist after setup");
        assert!(log.is_file(), "chat.log should exist after setup");
        // Result is Ok only when CHAT_TRACING_RELOAD is initialized (chat
        // subcommand path in main). Either outcome is non-panicking.
        match result {
            Ok(_guard) => { /* worker guard drops here → flush */ }
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("reload handle not initialized"),
                    "unexpected error variant: {msg}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_appends_does_not_truncate_existing_log() {
        let dir = unique_tmpdir("append");
        std::fs::create_dir_all(&dir).expect("test: create_dir_all must succeed");
        let log = dir.join("chat.log");
        std::fs::write(&log, b"pre-existing\n").expect("test: seed write must succeed");

        // Run setup; on Ok the guard drops immediately (flushing nothing
        // since nothing was logged); on Err we just confirm file is intact.
        let _ = setup_chat_tracing_to_file_in(&dir);

        let contents = std::fs::read_to_string(&log).expect("test: read log");
        assert!(
            contents.starts_with("pre-existing"),
            "OpenOptions::append must not truncate existing chat.log; got: {contents:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_returns_err_when_path_is_not_a_directory() {
        // Point `dir` at an existing file → create_dir_all should fail with
        // ENOTDIR / "Not a directory". Must return Err, must not panic.
        let parent = unique_tmpdir("notdir-parent");
        std::fs::create_dir_all(&parent).expect("test: create parent");
        let file_as_dir = parent.join("not-a-dir");
        std::fs::write(&file_as_dir, b"x").expect("test: seed file");

        let result = setup_chat_tracing_to_file_in(&file_as_dir);
        assert!(
            result.is_err(),
            "expected Err when target path is a regular file, got Ok"
        );
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn resolve_chat_log_dir_yields_an_absolute_path() {
        // Should succeed via either UserDirs or HOME; on container runners
        // without HOME the function bails — accept either outcome but never
        // panic.
        match resolve_chat_log_dir() {
            Ok(p) => {
                assert!(p.ends_with(".openprx"), "must point at ~/.openprx, got {p:?}");
                assert!(p.is_absolute(), "chat log dir must be absolute, got {p:?}");
            }
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("cannot determine home directory"),
                    "unexpected error: {msg}"
                );
            }
        }
    }
}

// ─── T3-3: ReduxMode 解析 + route_turn 真值表 ─────────────────────────────────

#[cfg(test)]
#[cfg(feature = "terminal-tui")]
mod redux_mode_tests {
    //! T3-3-a: `ReduxMode::parse` 覆盖 4 个枚举值 + fail-safe；
    //! T3-3-b: `route_turn` Pure → ReduxDriver 无需 driver_opt_in.
    //!
    //! 解析逻辑用 `ReduxMode::parse(&str)` 单测，避免依赖进程级 env state（不同测试
    //! 并行跑时 env 互相污染）。`from_env` 只是 env→parse 的薄包装，已由集成测试覆盖。
    use super::*;

    /// T3-3-a-1: Off 类输入（含空串 / 0 / off / legacy / 未识别）一律 Off
    #[test]
    fn parse_off_values() {
        assert_eq!(ReduxMode::parse(""), ReduxMode::Off);
        assert_eq!(ReduxMode::parse("0"), ReduxMode::Off);
        assert_eq!(ReduxMode::parse("off"), ReduxMode::Off);
        assert_eq!(ReduxMode::parse("OFF"), ReduxMode::Off);
        assert_eq!(ReduxMode::parse("Legacy"), ReduxMode::Off);
        // fail-safe：未识别值不升级到 Pure，防止误开 reducer 单路由
        assert_eq!(ReduxMode::parse("garbage"), ReduxMode::Off);
        assert_eq!(ReduxMode::parse("3"), ReduxMode::Off);
    }

    /// T3-3-a-2: "1" 与 "both"（大小写不敏感）都映射到 Both
    #[test]
    fn parse_both_values() {
        assert_eq!(ReduxMode::parse("1"), ReduxMode::Both);
        assert_eq!(ReduxMode::parse("both"), ReduxMode::Both);
        assert_eq!(ReduxMode::parse("BOTH"), ReduxMode::Both);
        assert_eq!(ReduxMode::parse(" both "), ReduxMode::Both, "trim 应生效");
    }

    /// T3-3-a-3: "redux" 显式别名（保留向后兼容）
    #[test]
    fn parse_redux_value() {
        assert_eq!(ReduxMode::parse("redux"), ReduxMode::Redux);
        assert_eq!(ReduxMode::parse("REDUX"), ReduxMode::Redux);
    }

    /// T3-3-a-4: "pure" / "2" → Pure（T3-3 收官模式）
    #[test]
    fn parse_pure_values() {
        assert_eq!(ReduxMode::parse("pure"), ReduxMode::Pure);
        assert_eq!(ReduxMode::parse("PURE"), ReduxMode::Pure);
        assert_eq!(ReduxMode::parse("2"), ReduxMode::Pure);
        assert_eq!(ReduxMode::parse(" pure "), ReduxMode::Pure, "trim 应生效");
    }

    /// T3-3-a-5: reducer_active / is_pure 辅助方法
    #[test]
    fn mode_helper_predicates() {
        assert!(!ReduxMode::Off.reducer_active());
        assert!(ReduxMode::Both.reducer_active());
        assert!(ReduxMode::Redux.reducer_active());
        assert!(ReduxMode::Pure.reducer_active());

        assert!(!ReduxMode::Off.is_pure());
        assert!(!ReduxMode::Both.is_pure());
        assert!(!ReduxMode::Redux.is_pure());
        assert!(ReduxMode::Pure.is_pure());
    }

    /// T3-3-b-1: Off / Both 任何 driver_opt_in 都走 legacy
    #[test]
    fn route_off_and_both_always_legacy() {
        for tools_empty in [false, true] {
            assert!(matches!(
                route_turn(ReduxMode::Off, false, tools_empty),
                TurnRoute::LegacyToolLoop
            ));
            assert!(matches!(
                route_turn(ReduxMode::Off, true, tools_empty),
                TurnRoute::LegacyToolLoop
            ));
            assert!(matches!(
                route_turn(ReduxMode::Both, false, tools_empty),
                TurnRoute::LegacyToolLoop
            ));
            assert!(matches!(
                route_turn(ReduxMode::Both, true, tools_empty),
                TurnRoute::LegacyToolLoop
            ));
        }
    }

    /// T3-3-b-2: Redux 需 driver_opt_in 才切到 driver（向后兼容 5a-4 语义）
    #[test]
    fn route_redux_requires_opt_in() {
        assert!(matches!(
            route_turn(ReduxMode::Redux, false, true),
            TurnRoute::LegacyToolLoop
        ));
        assert!(matches!(
            route_turn(ReduxMode::Redux, true, true),
            TurnRoute::ReduxDriver
        ));
    }

    /// T3-3-b-3: Pure 必走 driver，无论 driver_opt_in（T3-3 收官关键契约）
    #[test]
    fn route_pure_always_driver() {
        for opt_in in [false, true] {
            for tools_empty in [false, true] {
                assert!(
                    matches!(route_turn(ReduxMode::Pure, opt_in, tools_empty), TurnRoute::ReduxDriver),
                    "Pure mode must always route to ReduxDriver (opt_in={opt_in} tools_empty={tools_empty})"
                );
            }
        }
    }

    /// T3-3-fixA P0-2: Pure 模式跳过 legacy exit save_session 的真值表.
    ///
    /// 退出守卫表达式 `!ReduxMode::from_env().is_pure()`：
    /// - Off / Both / Redux → 守卫为 true，legacy save 兜底
    /// - Pure → 守卫为 false，跳过 legacy save，reducer 是唯一持久化源
    ///
    /// 这是 fixA P0-2 修复的逻辑契约——直接断言 is_pure() 真值表，避免依赖 env state.
    #[test]
    fn pure_mode_skips_legacy_exit_save_via_redux_mode_guard() {
        // Pure 是唯一应跳过 legacy exit save 的模式
        assert!(ReduxMode::Pure.is_pure(), "Pure.is_pure() 必须 true");
        assert!(!ReduxMode::Off.is_pure(), "Off.is_pure() 必须 false");
        assert!(!ReduxMode::Both.is_pure(), "Both.is_pure() 必须 false");
        assert!(!ReduxMode::Redux.is_pure(), "Redux.is_pure() 必须 false");

        // 守卫表达式 `legacy_exit_save_enabled = !mode.is_pure()` 真值
        for (mode, expected_legacy_enabled) in [
            (ReduxMode::Off, true),
            (ReduxMode::Both, true),
            (ReduxMode::Redux, true),
            (ReduxMode::Pure, false),
        ] {
            let legacy_exit_save_enabled = !mode.is_pure();
            assert_eq!(
                legacy_exit_save_enabled, expected_legacy_enabled,
                "mode {mode:?}: legacy_exit_save_enabled 应为 {expected_legacy_enabled}"
            );
        }
    }

    /// T3-3-fixB B4: Pure 模式跳过 legacy `chat_session.set_mode` 的真值表.
    ///
    /// SetMode 命令分支的守卫表达式 `legacy_session_mode_writes_enabled = !ReduxMode::from_env().is_pure()`:
    /// - Off / Both / Redux → 守卫为 true，legacy set_mode 跑
    /// - Pure → 守卫为 false，跳过 legacy set_mode（chat_session 不参与持久化，写 mode 是死写）
    ///
    /// 与 mod.rs:2197 `legacy_session_writes_enabled` 同形结构.
    #[test]
    fn pure_mode_skips_legacy_set_mode_via_redux_mode_guard() {
        for (mode, expected_legacy_enabled) in [
            (ReduxMode::Off, true),
            (ReduxMode::Both, true),
            (ReduxMode::Redux, true),
            (ReduxMode::Pure, false),
        ] {
            let legacy_session_mode_writes_enabled = !mode.is_pure();
            assert_eq!(
                legacy_session_mode_writes_enabled, expected_legacy_enabled,
                "mode {mode:?}: legacy_session_mode_writes_enabled 应为 {expected_legacy_enabled}"
            );
        }
    }
}
