//! Redux-like ChatState 及其 reducer.
//!
//! 包含:
//! - [`ChatState`] — 顶层状态，持有 4 个子结构
//! - [`SessionState`] / [`UiState`] / [`StreamState`] / [`ControlState`]
//! - [`Effect`] — reduce 返回的副作用指令，由主循环的 async 外壳执行
//!
//! 设计原则:
//! - `ChatState::reduce` 是纯 sync 函数，无 I/O，无 await，只 mutate 自身
//! - Effect 是 enum（非 Box<dyn FnOnce>），Send + Sync，可序列化/可测试
//! - ChatState 由单一 owner（主循环）持有，不需要 Arc<Mutex<>>

// Step 2: 接入真实类型 — TuiInput / ConversationLine / StreamingDraft 来自
// `crate::chat::tui`（feature = "terminal-tui"）。非 TUI feature 下沿用占位
// 类型以确保两套 feature 均可独立编译。
//
// 注意：UiState 中 `input` 是 reducer 的输入缓冲快照（new path 写入），
// 而旧路径仍把按键转发给 `chat_mirror.lock().input`（TuiState 内嵌 TuiInput）。
// Step 5 删旧路径后 chat_mirror 即被 ChatState.ui 取代。

#[cfg(feature = "terminal-tui")]
pub use crate::chat::tui::{ConversationLine, SlashMenuState, StreamingDraft, TuiInput};

/// 占位：TuiInput（非 terminal-tui feature；保持 reducer 在最小 feature 下也能编译）
#[cfg(not(feature = "terminal-tui"))]
pub type TuiInput = Vec<String>;

/// 占位：ConversationLine（非 terminal-tui feature）
#[cfg(not(feature = "terminal-tui"))]
pub type ConversationLine = String;

/// 占位：SlashMenuState（非 terminal-tui feature）
#[cfg(not(feature = "terminal-tui"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashMenuState;

/// 占位：StreamingDraft（非 terminal-tui feature）
#[cfg(not(feature = "terminal-tui"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamingDraft {
    pub draft_id: String,
    pub accumulated: String,
    pub version: u64,
}

use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::agent::loop_::ChatMode;
use crate::channels::traits::SendMessage;
use crate::chat::action::{
    Action, CompactReason, HistoryDir, MainQueueStatus, ProviderUsageRecordKind, ProviderWorkerStatus,
};
use crate::chat::session::{ChatSession, ChatTurn, MainSessionTokenUsageRecord, MainSessionTokenUsageSummary};
use crate::chat::slash_types::AtPathCandidate;
use crate::hooks::HookEvent;
use crate::memory::MemoryCategory;
use crate::providers::ChatMessage;
use crate::security::AutonomyLevel;
use crate::util::truncate_with_ellipsis;

/// S2-B Step 1: `Action::HistoryCompacted` reducer 对齐 `chat::mod::compact_chat_history`
/// 的常量边界。三个常量必须与 `chat::mod` 同源以确保两条路径在双写期产生相同结果；
/// 后续 step 删除旧路径时直接用 reducer 这套即可。
const COMPACT_KEEP_MESSAGES: usize = 8;
const COMPACT_CONTENT_CHARS: usize = 320;
const COMPACT_TOTAL_CHARS: usize = 2400;

#[cfg(feature = "terminal-tui")]
fn conversation_lines_from_turns(turns: &[ChatTurn]) -> Vec<ConversationLine> {
    turns
        .iter()
        .filter_map(|turn| match turn.role.as_str() {
            "user" => Some(ConversationLine::User {
                content: turn.content.clone(),
            }),
            "assistant" => Some(ConversationLine::Assistant {
                content: turn.content.clone(),
            }),
            "system" => Some(ConversationLine::System {
                content: turn.content.clone(),
            }),
            _ => None,
        })
        .collect()
}

#[cfg(not(feature = "terminal-tui"))]
fn conversation_lines_from_turns(turns: &[ChatTurn]) -> Vec<ConversationLine> {
    turns
        .iter()
        .filter(|turn| matches!(turn.role.as_str(), "user" | "assistant" | "system"))
        .map(|turn| turn.content.clone())
        .collect()
}

fn is_durable_compaction_history_message(message: &ChatMessage) -> bool {
    matches!(message.role.as_str(), "user" | "assistant")
        && !message.content.starts_with("[Post-compaction context refresh]")
}

fn durable_turns_from_compacted_history(history: &[ChatMessage]) -> Vec<ChatTurn> {
    let timestamp = chrono::Utc::now();
    history
        .iter()
        .filter(|message| is_durable_compaction_history_message(message))
        .map(|message| ChatTurn {
            role: message.role.clone(),
            content: message.content.clone(),
            timestamp,
            tool_calls: Vec::new(),
        })
        .collect()
}

// ─── Effect ──────────────────────────────────────────────────────────────────

/// Effect = 必须由 async 外壳执行的副作用.
///
/// 设计为 enum 而非 `Box<dyn FnOnce>`:
/// - `Send + Sync` 天然满足
/// - 可序列化用于日志/replay/test snapshot
/// - 无堆分配（除内置 String/Arc）
///
/// 由 [`ChatState::reduce`] 返回，交给主循环 dispatch。
#[allow(dead_code)]
#[derive(Debug)]
pub enum Effect {
    /// 开始新一轮 LLM 推理：传入 draft_id、history 快照、取消令牌.
    ///
    /// `draft_id` 由 [`Action::TurnStarted`] 携带并写入 `state.stream.draft`；
    /// 执行器子任务用它给 `StreamChunkReceived` / `StreamCompleted` 等 Action
    /// 打标记，reducer 才能匹配并合并 delta（见 `state.rs::reduce_stream_chunk_received`）。
    /// Step 5a-2 起 `EffectExecutor` 在 deps 模式下真调 `provider.stream_chat_with_history`，
    /// 流式 chunk 通过 `EffectDeps::action_tx` 回投到 reducer。
    StartTurn {
        /// Main turn scheduler identity for the real provider execution task.
        provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        draft_id: String,
        history: Vec<ChatMessage>,
        /// Optional persisted/original-history source used for compaction patch
        /// guard identity. When absent, the driver uses `history` for both budget
        /// and guard source, preserving non-chat/test callers.
        compaction_guard_history: Option<Vec<ChatMessage>>,
        /// P5 proactive budgeting config resolved from the selected model
        /// window. The streaming driver uses it for preflight/mid-turn trims.
        compaction_config: Option<crate::config::AgentCompactionConfig>,
        cancel: CancellationToken,
        /// BUG-09: the interactive chat mode in effect for this turn. The driver
        /// (`drive_start_turn_stream`) uses it to intercept write/shell/git
        /// tools when in [`ChatMode::Plan`] and feed back a simulated
        /// "[plan mode] would call X" result instead of executing them.
        chat_mode: ChatMode,
        /// D8-4 (redux path): the turn-root spawn execution context for this
        /// turn (forwarded from `Action::StartLLMTurn`). The `EffectExecutor`
        /// wraps `drive_start_turn_stream` in `SPAWN_EXECUTION_CONTEXT.scope(..)`
        /// with this value so `sessions_spawn` tool calls inside the turn inherit
        /// `parent_run_id = turn run_id` → origin = Model, mirroring the legacy
        /// `run_tool_call_loop_traced` wrapper in `chat::run`. `None` → no scope
        /// (sub-agents fall back to user origin, correct for non-turn callers).
        turn_spawn_ctx: Option<crate::tools::sessions_spawn::SpawnExecutionContext>,
        /// Per-turn default route for `message_send` tool calls. `None` keeps
        /// non-turn/test callers on the tool's legacy fallback slot.
        turn_message_send_ctx: Option<crate::tools::message_send::MessageSendExecutionContext>,
    },
    /// 持久化当前会话快照
    SaveSession(ChatSession),
    /// 通知渲染层 draft 已完成，推入 conversation_lines
    SendDraftFinalize { draft_id: String, text: String },
    /// 取消指定 draft 的 streaming
    CancelDraft(String),
    /// S2-B Step 2: 真正调用 `CancellationToken::cancel()` 取消当前 turn 的 LLM/工具流.
    ///
    /// 与 [`Self::CancelDraft`] 的区别:
    /// - `CancelDraft` 仅通知 channel 撤销 draft UI（用户看到的 streaming 块停止追加）
    /// - `CancelToken` 真触发底层 token cancel，让 `run_tool_call_loop` /
    ///   `drive_start_turn_stream` 立刻返回 cancelled 错误
    ///
    /// reducer 在 `reduce_cancel_requested` 内**收集** active_cancel.take() 后构造此
    /// Effect；EffectExecutor 在 real 模式下直接 `token.cancel()`，shadow 模式下记
    /// debug log。这关闭了 S2-B Codex 风险中 "UI 取消了但底层仍跑" 的窗口。
    CancelToken(CancellationToken),
    /// 向 channel 发送消息（槽命令输出等）
    EmitChannelMessage(SendMessage),
    /// 写入 memory backend
    PersistToMemory {
        key: String,
        value: String,
        category: MemoryCategory,
    },
    /// 触发 hook 事件
    NotifyHook {
        event: HookEvent,
        payload: serde_json::Value,
    },
    /// 请求 TUI 重绘一帧
    RequestRedraw,
    /// 展示媒体内容（图像/音频等）
    DisplayMedia { kind: String, path: String },
    /// 自动为会话生成标题
    AutoTitleSession(String),
    /// 结构化 trace 日志
    LogTrace { level: tracing::Level, msg: String },
    /// **S3 T3-1**: EffectExecutor 把 approval 请求转发到 UI / CLI prompt.
    ///
    /// driver 在执行需 approval 的 tool 前 dispatch [`Action::ToolApprovalRequested`]；
    /// reducer 据此产生本 Effect；EffectExecutor 在 real 模式下负责把请求转给
    /// UI 渲染层 / CLI prompt（当前 stub：log + 默认 approve），由 UI 在用户响应后
    /// 回投 [`Action::ToolApprovalReceived`]。
    ///
    /// 数据流为单向 fire-and-forget（driver 通过 `approval_response_tx` mpsc 反向
    /// 接收响应）。Effect 不要求响应。
    RequestApproval {
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        tool_id: String,
        name: String,
        args: String,
    },
    /// Resolve a pending foreground approval without going through a separate
    /// `ToolApprovalReceived` Action. Used by pure key handling paths where the
    /// reducer owns the key event and must return the approval decision to the
    /// dispatcher/executor as an effect.
    ResolveApproval { tool_id: String, approved: bool },
    /// 优雅退出主循环
    Quit,
}

impl Effect {
    /// S2.5 T2.5-2: 取 Effect 变体名作为 `'static str` 用于 Prometheus label.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::StartTurn { .. } => "StartTurn",
            Self::SaveSession(_) => "SaveSession",
            Self::SendDraftFinalize { .. } => "SendDraftFinalize",
            Self::CancelDraft(_) => "CancelDraft",
            Self::CancelToken(_) => "CancelToken",
            Self::EmitChannelMessage(_) => "EmitChannelMessage",
            Self::PersistToMemory { .. } => "PersistToMemory",
            Self::NotifyHook { .. } => "NotifyHook",
            Self::RequestRedraw => "RequestRedraw",
            Self::DisplayMedia { .. } => "DisplayMedia",
            Self::AutoTitleSession(_) => "AutoTitleSession",
            Self::LogTrace { .. } => "LogTrace",
            Self::RequestApproval { .. } => "RequestApproval",
            Self::ResolveApproval { .. } => "ResolveApproval",
            Self::Quit => "Quit",
        }
    }
}

// ─── Sub-states ───────────────────────────────────────────────────────────────

/// 会话持久化相关状态（写入 memory backend）.
#[allow(dead_code)]
pub struct SessionState {
    /// 会话唯一 ID
    pub id: String,
    /// 会话标题（自动生成或用户设置）
    pub title: String,
    /// 当前 provider 名（整个 session 不变，Arc<str> 减少 clone）
    pub provider: Arc<str>,
    /// 当前 model 名
    pub model: Arc<str>,
    /// 交互模式（plan/edit/auto）
    pub mode: ChatMode,
    /// 完整对话回合（持久化用）
    pub turns: Vec<ChatTurn>,
    /// LLM 上下文消息列表（system+user+assistant，用于下一次请求）
    pub history: Vec<ChatMessage>,
    /// 会话创建时间（首次 RecordUserTurn 时延迟初始化；build_session_snapshot 不再覆盖）
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
    /// 本会话内运行过的后台 session（agent/shell/pty）摘要（v4）。仅持久化摘要，
    /// reload 时还原用于展示——绝不重建进程/sub-agent/PTY。由主循环经
    /// `Action::BackgroundSessionRecorded` 写入（去重 by id），随
    /// `build_session_snapshot` 落盘，`reduce_session_loaded` 还原。
    pub background_sessions: Vec<crate::chat::sessions::PersistedSessionSummary>,
    /// Main-session token records, success-only. Child-session usage is Phase 4.
    pub token_usage_records: Vec<MainSessionTokenUsageRecord>,
}

/// TUI UI 临时状态（退出即弃，不持久化）.
///
/// Step 2 起接入真实 `TuiInput`/`ConversationLine`（feature = "terminal-tui"）；
/// 非 TUI feature 下使用占位类型保持编译兼容。
#[allow(dead_code)]
pub struct UiState {
    /// 渲染好的对话行
    pub conversation_lines: Vec<ConversationLine>,
    /// Incremented when conversation_lines is replaced wholesale.
    pub conversation_generation: u64,
    /// 多行输入 buffer + 历史
    pub input: TuiInput,
    /// 当前对话回合计数（用于状态栏）
    pub turn_count: usize,
    /// In-session chat mode displayed in the status bar.
    pub chat_mode: ChatMode,
    /// Configured autonomy ceiling displayed in the status bar. This is read-only
    /// UI metadata and does not mutate the security policy.
    pub autonomy_level: AutonomyLevel,
    /// 是否启用 ASCII 降级（非 UTF-8 终端）
    pub ascii_fallback: bool,
    /// 上次 Ctrl+C 的时间戳（ms），用于双击窗口判断
    pub last_ctrlc_ms: u64,
    /// 最近一次输入提交（reducer 内 KeyPressed::Enter 时由 reduce 自身派生
    /// `Action::InputSubmitted`；该字段用于测试断言双写期最后一次提交内容）
    pub last_submitted: Option<String>,
    /// 后台会话常驻状态行（v1b）。空字符串表示无后台会话（renderer 隐藏该行）。
    /// 仅由 chat 主循环经 `Action::SessionsStatusUpdated` 写入；后台 spawn 任务
    /// 绝不触碰（铁律：state 只在主循环写）。
    pub sessions_status: String,
    /// P1 sessions strip entries. This is the same child TUI registry snapshot
    /// used by the Ctrl+G switcher, kept structured for rendering.
    pub sessions_entries: Vec<crate::chat::sessions::SwitcherEntry>,
    /// Main-session input backlog status for orchestration observation.
    pub main_queue_status: MainQueueStatus,
    /// Main-session provider worker status for orchestration observation.
    pub provider_worker_status: ProviderWorkerStatus,
    /// Saved chat-session candidates for `/resume` slash-menu arguments.
    pub saved_sessions_cache: Vec<crate::chat::session::SavedSessionPickerEntry>,
    /// Provider/model candidates for `/provider` and `/model` slash-menu args.
    pub provider_model_catalog: Vec<crate::chat::slash_types::SlashProviderModelCatalog>,
    /// P2 active line-session viewport snapshot. `None` when the main chat or a
    /// PTY handoff owns the visible surface.
    pub active_session_view: Option<crate::chat::sessions::ActiveSessionView>,
    /// P6c1 foreground tool approval prompt. Display-only; the dispatcher
    /// ApprovalRouter remains the single execution gate.
    pub pending_tool_approval: Option<crate::chat::sessions::PendingToolApprovalView>,
    /// Current context-budget numerator used by status bar budget display.
    /// Derived from the planned prompt context, not cumulative session usage.
    pub context_used_tokens: Option<usize>,
    /// Effective context window used by status bar budget display.
    pub context_window_tokens: Option<usize>,
    /// Main-session cumulative token/cost summary for the status bar.
    pub token_usage_summary: MainSessionTokenUsageSummary,
    /// 当前输入路由目标（v1.1b）。`Main` = 主 chat；`Session{seq}` = 已 attach
    /// 的后台 session（输入作为 steer）。由 chat 主循环经
    /// `Action::SessionFocusChanged` 在 /attach//detach 时写入；驱动提示符的
    /// 颜色+字形目标指示。
    pub focus: crate::chat::sessions::FocusTarget,
    /// Ctrl+G session switcher 弹层状态（v1.1b），关闭时为 `None`。由 key 线程
    /// 经 `Action::SwitcherOpened` / `SwitcherMoved` / `SwitcherClosed` 写入。
    pub switcher: Option<crate::chat::sessions::SwitcherState>,
    /// UI-only bottom-strip selection for direct Alt+arrow navigation. Separate
    /// from `focus`: this highlights an entry but does not route input there.
    pub strip_selection: Option<u64>,
    /// Slash-command menu overlay. Derived from the current input command token.
    pub slash_menu: Option<SlashMenuState>,
    /// Security-filtered `@path` completion source, delivered via Action.
    pub at_path_candidates: Vec<AtPathCandidate>,
    /// P7c saved chat-session history picker. Distinct from the child-TUI
    /// Ctrl+G switcher.
    pub saved_session_picker: Option<crate::chat::session::SavedSessionPickerState>,
}

/// 不可变 UI 快照（renderer 仅读，dispatcher 在 ui_dirty=true 时构造）.
///
/// S4-A Commit 1: 引入 UiSnapshot 作为 reducer 与 ratatui 渲染线程之间的
/// 单向只读通道。Arc 字段共享让"每轮 push 一行"不需要 clone 整个
/// `Vec<ConversationLine>`；revision 单调递增供 watch::Sender::send_if_modified
/// 跳过相同帧 + 调试断言。
///
/// 字段对应 fullscreen renderer 需要的最小集（status bar / transcript /
/// input 框 / footer）；BottomChromeView trait（Commit 2 落地）抽象掉
/// TuiState vs UiSnapshot 的差异。
#[cfg(feature = "terminal-tui")]
#[derive(Clone)]
#[allow(dead_code)]
pub struct UiSnapshot {
    /// 单调递增，watch::Sender::send_if_modified 用于跳过相同帧.
    pub revision: u64,
    /// 当前 provider 名（status bar 显示）.
    pub provider: Arc<str>,
    /// 当前 model 名.
    pub model: Arc<str>,
    /// In-session chat mode displayed in the status bar.
    pub chat_mode: ChatMode,
    /// Configured autonomy ceiling displayed in the status bar.
    pub autonomy_level: AutonomyLevel,
    /// 会话标题（status bar 显示）.
    pub session_title: Arc<str>,
    /// 对话回合计数（status bar 显示）.
    pub turn_count: usize,
    /// ASCII 降级模式标志.
    pub ascii_fallback: bool,
    /// 对话行历史（fullscreen transcript renderer 使用）.
    pub conversation_lines: Arc<Vec<ConversationLine>>,
    /// Generation marker for wholesale conversation history replacement.
    pub conversation_generation: u64,
    /// 当前 in-flight streaming draft（None 表示空闲）.
    pub streaming: Option<StreamingDraft>,
    /// In-flight visible streaming drafts keyed by provider worker sequence.
    pub visible_streaming_drafts: Arc<Vec<VisibleStreamingDraftView>>,
    /// 输入 buffer 快照（clone 成本接受，多行场景 < INPUT_MAX_VISIBLE_ROWS）.
    pub input: TuiInput,
    /// 后台会话常驻状态行（v1b）。空字符串表示无后台会话（renderer 隐藏该行）。
    pub sessions_status: Arc<str>,
    /// P1 sessions strip entries, cloned from reducer-owned UI state.
    pub sessions_entries: Arc<Vec<crate::chat::sessions::SwitcherEntry>>,
    /// Main-session input backlog status.
    pub main_queue_status: MainQueueStatus,
    /// Main-session provider worker status.
    pub provider_worker_status: ProviderWorkerStatus,
    /// P2 active line-session viewport snapshot.
    pub active_session_view: Option<crate::chat::sessions::ActiveSessionView>,
    /// P6c1 foreground tool approval prompt.
    pub pending_tool_approval: Option<crate::chat::sessions::PendingToolApprovalView>,
    /// Current context-budget numerator for UI-only status budget display.
    pub context_used_tokens: Option<usize>,
    /// Effective context window for UI-only status budget display.
    pub context_window_tokens: Option<usize>,
    /// Main-session cumulative token/cost summary for the status bar.
    pub token_usage_summary: MainSessionTokenUsageSummary,
    /// 当前输入路由目标（v1.1b）。驱动提示符颜色+字形指示。
    pub focus: crate::chat::sessions::FocusTarget,
    /// Ctrl+G switcher 弹层（v1.1b），`None` 表示关闭。renderer 据此画弹层。
    pub switcher: Option<crate::chat::sessions::SwitcherState>,
    /// UI-only bottom-strip selection. `None` means no highlighted strip entry.
    pub strip_selection: Option<u64>,
    /// Slash-command menu overlay.
    pub slash_menu: Option<SlashMenuState>,
    /// P7c saved chat-session history picker overlay.
    pub saved_session_picker: Option<crate::chat::session::SavedSessionPickerState>,
}

#[cfg(feature = "terminal-tui")]
impl UiSnapshot {
    /// 构造空快照（revision=0，仅 provider/model 已知，session 未加载）.
    #[must_use]
    #[allow(dead_code)]
    pub fn initial(provider: Arc<str>, model: Arc<str>) -> Self {
        Self {
            revision: 0,
            provider,
            model,
            chat_mode: ChatMode::default(),
            autonomy_level: AutonomyLevel::default(),
            session_title: Arc::from(""),
            turn_count: 0,
            ascii_fallback: false,
            conversation_lines: Arc::new(Vec::new()),
            conversation_generation: 0,
            streaming: None,
            visible_streaming_drafts: Arc::new(Vec::new()),
            input: TuiInput::new(),
            sessions_status: Arc::from(""),
            sessions_entries: Arc::new(Vec::new()),
            main_queue_status: MainQueueStatus::default(),
            provider_worker_status: ProviderWorkerStatus::default(),
            active_session_view: None,
            pending_tool_approval: None,
            context_used_tokens: None,
            context_window_tokens: None,
            token_usage_summary: MainSessionTokenUsageSummary::default(),
            focus: crate::chat::sessions::FocusTarget::Main,
            switcher: None,
            strip_selection: None,
            slash_menu: None,
            saved_session_picker: None,
        }
    }
}

#[cfg(feature = "terminal-tui")]
impl UiSnapshot {
    #[must_use]
    pub fn streaming_draft_for_worker(&self, sequence: u64) -> Option<&StreamingDraft> {
        self.visible_streaming_drafts
            .iter()
            .find(|draft| draft.sequence == sequence)
            .map(|draft| &draft.draft)
    }
}

/// One keyed visible streaming turn draft.
///
/// Phase 1 is structural only: the live chat loop still keeps visible provider
/// turns safe-serial via admission guard, but the reducer can now represent
/// multiple drafts without a single global draft slot.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamingTurnDraft {
    pub task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
    /// Scheduler sequence; lower sequence renders as the primary/earlier draft.
    pub sequence: u64,
    pub prompt_preview: String,
    pub draft: StreamingDraft,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleStreamingDraftView {
    pub sequence: u64,
    pub draft: StreamingDraft,
}

/// 流式推理中间态（每轮重置）.
#[allow(dead_code)]
pub struct StreamState {
    /// Keyed in-flight visible streaming drafts.
    ///
    /// This is the single source of truth for streaming draft state. The legacy
    /// single-draft snapshot is computed from [`Self::primary_draft`].
    pub visible_drafts: Vec<StreamingTurnDraft>,
}

impl StreamState {
    #[must_use]
    pub fn primary_draft(&self) -> Option<&StreamingTurnDraft> {
        self.visible_drafts.first()
    }

    #[must_use]
    pub fn primary_streaming_draft(&self) -> Option<&StreamingDraft> {
        self.primary_draft().map(|turn| &turn.draft)
    }

    #[must_use]
    pub fn streaming_draft_for_worker(&self, sequence: u64) -> Option<&StreamingDraft> {
        self.visible_drafts
            .iter()
            .find(|turn| turn.sequence == sequence)
            .map(|turn| &turn.draft)
    }

    #[must_use]
    fn visible_streaming_draft_views(&self) -> Vec<VisibleStreamingDraftView> {
        self.visible_drafts
            .iter()
            .map(|turn| VisibleStreamingDraftView {
                sequence: turn.sequence,
                draft: turn.draft.clone(),
            })
            .collect()
    }

    fn insert_visible_draft(&mut self, draft: StreamingTurnDraft) {
        self.visible_drafts
            .retain(|existing| existing.draft.draft_id != draft.draft.draft_id);
        let insert_at = self
            .visible_drafts
            .iter()
            .position(|existing| existing.sequence > draft.sequence)
            .unwrap_or(self.visible_drafts.len());
        self.visible_drafts.insert(insert_at, draft);
    }

    fn visible_draft_mut(&mut self, draft_id: &str) -> Option<&mut StreamingTurnDraft> {
        self.visible_drafts
            .iter_mut()
            .find(|turn| turn.draft.draft_id == draft_id)
    }

    fn remove_visible_draft(&mut self, draft_id: &str) -> Option<StreamingTurnDraft> {
        let idx = self
            .visible_drafts
            .iter()
            .position(|turn| turn.draft.draft_id == draft_id)?;
        Some(self.visible_drafts.remove(idx))
    }

    fn clear_visible_drafts(&mut self) {
        self.visible_drafts.clear();
    }

    #[must_use]
    const fn has_visible_drafts(&self) -> bool {
        !self.visible_drafts.is_empty()
    }

    #[must_use]
    fn versions_fingerprint(&self) -> Vec<(String, u64)> {
        self.visible_drafts
            .iter()
            .map(|turn| (turn.draft.draft_id.clone(), turn.draft.version))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolTaskKey {
    Task(crate::chat::turn_scheduler::TurnTaskId),
    Primary,
}

impl ToolTaskKey {
    #[must_use]
    pub const fn from_task_id(task_id: Option<crate::chat::turn_scheduler::TurnTaskId>) -> Self {
        match task_id {
            Some(id) => Self::Task(id),
            None => Self::Primary,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ToolInvocationKey {
    pub tool_call_id: Option<String>,
    pub name: String,
}

impl ToolInvocationKey {
    #[must_use]
    fn new(tool_call_id: Option<String>, name: &str) -> Self {
        Self {
            tool_call_id,
            name: name.to_string(),
        }
    }
}

#[derive(Default, Debug)]
pub struct TaskToolBuffer {
    pub pending_tool_cards: Vec<usize>,
    pub tool_calls: Vec<crate::chat::session::ToolCallSummary>,
    pub tool_args: std::collections::HashMap<ToolInvocationKey, String>,
}

/// 取消/关停控制状态.
#[allow(dead_code)]
pub struct ControlState {
    /// 当前回合的取消令牌（None 表示空闲）
    pub active_cancel: Option<CancellationToken>,
    /// 全局关停令牌（长生命周期，跨任务共享）
    pub shutdown: CancellationToken,
    /// 是否正在生成（用于 CancelRequested 分支判断）
    pub generating: bool,
    /// P3a: tool state is keyed by turn task so concurrent visible workers do
    /// not share running card indices, argument previews, or persisted summaries.
    pub tool_buffers: std::collections::HashMap<ToolTaskKey, TaskToolBuffer>,
    /// P3b: graceful provider cancellation tokens keyed by turn task. Legacy
    /// Primary callers keep using `active_cancel` until the runtime fully
    /// migrates away from the pre-scheduler path.
    pub turn_cancels: std::collections::HashMap<crate::chat::turn_scheduler::TurnTaskId, CancellationToken>,
    /// P3c: final aggregate usage records are idempotent per provider task.
    /// Incremental usage records are intentionally never tracked here.
    pub final_usage_tasks_recorded: std::collections::HashSet<crate::chat::turn_scheduler::TurnTaskId>,
}

impl ControlState {
    fn register_turn_cancel(&mut self, key: ToolTaskKey, cancel: CancellationToken) {
        match key {
            ToolTaskKey::Task(task_id) => {
                self.turn_cancels.insert(task_id, cancel);
            }
            ToolTaskKey::Primary => {
                self.active_cancel = Some(cancel);
            }
        }
    }

    fn take_turn_cancel(&mut self, key: ToolTaskKey) -> Option<CancellationToken> {
        match key {
            ToolTaskKey::Task(task_id) => self.turn_cancels.remove(&task_id),
            ToolTaskKey::Primary => self.active_cancel.take(),
        }
    }

    fn remove_turn_cancel(&mut self, key: ToolTaskKey) {
        match key {
            ToolTaskKey::Task(task_id) => {
                self.turn_cancels.remove(&task_id);
            }
            ToolTaskKey::Primary => {
                self.active_cancel = None;
            }
        }
    }

    fn has_task_turn_cancels(&self) -> bool {
        !self.turn_cancels.is_empty()
    }

    fn drain_turn_cancels(&mut self) -> Vec<CancellationToken> {
        let mut tokens = Vec::new();
        if let Some(token) = self.active_cancel.take() {
            tokens.push(token);
        }
        tokens.extend(self.turn_cancels.drain().map(|(_, token)| token));
        tokens
    }

    fn should_record_provider_usage(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        usage_kind: ProviderUsageRecordKind,
    ) -> bool {
        match (task_id, usage_kind) {
            (Some(task_id), ProviderUsageRecordKind::FinalAggregate) => self.final_usage_tasks_recorded.insert(task_id),
            _ => true,
        }
    }

    fn tool_buffer_mut(&mut self, key: ToolTaskKey) -> &mut TaskToolBuffer {
        self.tool_buffers.entry(key).or_default()
    }

    fn take_tool_calls(&mut self, key: ToolTaskKey) -> Vec<crate::chat::session::ToolCallSummary> {
        let Some(buffer) = self.tool_buffers.get_mut(&key) else {
            return Vec::new();
        };
        let calls = std::mem::take(&mut buffer.tool_calls);
        buffer.tool_args.clear();
        let remove_buffer =
            buffer.pending_tool_cards.is_empty() && buffer.tool_calls.is_empty() && buffer.tool_args.is_empty();
        if remove_buffer {
            self.tool_buffers.remove(&key);
        }
        calls
    }

    fn clear_tool_buffer(&mut self, key: ToolTaskKey) {
        self.tool_buffers.remove(&key);
    }

    fn clear_all_tool_buffers(&mut self) {
        self.tool_buffers.clear();
    }

    #[cfg(test)]
    fn pending_tool_card_count(&self, key: ToolTaskKey) -> usize {
        self.tool_buffers
            .get(&key)
            .map_or(0, |buffer| buffer.pending_tool_cards.len())
    }

    #[cfg(test)]
    fn tool_call_count(&self, key: ToolTaskKey) -> usize {
        self.tool_buffers.get(&key).map_or(0, |buffer| buffer.tool_calls.len())
    }

    #[cfg(test)]
    fn tool_arg_count(&self, key: ToolTaskKey) -> usize {
        self.tool_buffers.get(&key).map_or(0, |buffer| buffer.tool_args.len())
    }
}

// ─── ChatState ────────────────────────────────────────────────────────────────

/// 顶层聊天状态，由主循环单一 owner 持有.
///
/// 不使用 `Arc<Mutex<ChatState>>`；renderer 通过快照 channel 接收只读副本。
/// 所有变更通过 [`ChatState::reduce`] 统一应用。
#[allow(dead_code)]
pub struct ChatState {
    /// 持久化会话状态
    pub session: SessionState,
    /// TUI UI 临时状态
    pub ui: UiState,
    /// 流式中间态
    pub stream: StreamState,
    /// 取消/关停控制
    pub control: ControlState,
    /// build_ui_snapshot 的 conversation_lines Arc 缓存，dirty 时清空
    #[cfg(feature = "terminal-tui")]
    cached_lines_arc: Option<Arc<Vec<ConversationLine>>>,
}

#[cfg(feature = "terminal-tui")]
#[derive(Debug, PartialEq)]
struct SnapshotDirtyFields {
    conversation_len: usize,
    conversation_generation: u64,
    draft_versions: Vec<(String, u64)>,
    input_lines: usize,
    context_used_tokens: Option<usize>,
    context_window_tokens: Option<usize>,
    slash_menu_open: bool,
    slash_menu_selected: Option<usize>,
    chat_mode: ChatMode,
    autonomy_level: AutonomyLevel,
    approval_visible: bool,
    focus: crate::chat::sessions::FocusTarget,
    token_usage_summary: MainSessionTokenUsageSummary,
    main_queue_status: MainQueueStatus,
}

impl ChatState {
    /// 构造初始状态（合理默认值）.
    ///
    /// `provider`/`model` 传入 Arc<str> 以避免后续 clone。
    /// `shutdown` 由调用方创建并共享给所有子任务。
    pub fn new(provider: Arc<str>, model: Arc<str>, shutdown: CancellationToken) -> Self {
        Self {
            session: SessionState {
                id: uuid::Uuid::new_v4().to_string(),
                title: String::new(),
                provider,
                model,
                mode: ChatMode::default(),
                turns: Vec::new(),
                history: Vec::new(),
                created_at: None,
                background_sessions: Vec::new(),
                token_usage_records: Vec::new(),
            },
            ui: UiState {
                conversation_lines: Vec::new(),
                conversation_generation: 0,
                input: Self::new_input(),
                turn_count: 0,
                chat_mode: ChatMode::default(),
                autonomy_level: AutonomyLevel::default(),
                ascii_fallback: false,
                last_ctrlc_ms: 0,
                last_submitted: None,
                sessions_status: String::new(),
                sessions_entries: Vec::new(),
                main_queue_status: MainQueueStatus::default(),
                provider_worker_status: ProviderWorkerStatus::default(),
                saved_sessions_cache: Vec::new(),
                provider_model_catalog: Vec::new(),
                active_session_view: None,
                pending_tool_approval: None,
                context_used_tokens: None,
                context_window_tokens: None,
                token_usage_summary: MainSessionTokenUsageSummary::default(),
                focus: crate::chat::sessions::FocusTarget::Main,
                switcher: None,
                strip_selection: None,
                slash_menu: None,
                at_path_candidates: Vec::new(),
                saved_session_picker: None,
            },
            stream: StreamState {
                visible_drafts: Vec::new(),
            },
            control: ControlState {
                active_cancel: None,
                shutdown,
                generating: false,
                tool_buffers: std::collections::HashMap::new(),
                turn_cancels: std::collections::HashMap::new(),
                final_usage_tasks_recorded: std::collections::HashSet::new(),
            },
            #[cfg(feature = "terminal-tui")]
            cached_lines_arc: None,
        }
    }

    /// 构造空 TuiInput / 占位 Vec（与当前 feature 匹配）.
    #[cfg(feature = "terminal-tui")]
    fn new_input() -> TuiInput {
        TuiInput::new()
    }

    /// 非 terminal-tui feature 下使用占位 Vec.
    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::missing_const_for_fn)]
    fn new_input() -> TuiInput {
        Vec::new()
    }

    #[cfg(feature = "terminal-tui")]
    const fn slash_menu_sources_from<'a>(
        live_sessions: &'a [crate::chat::sessions::SwitcherEntry],
        saved_sessions: &'a [crate::chat::session::SavedSessionPickerEntry],
        provider_model_catalog: &'a [crate::chat::tui::SlashProviderModelCatalog],
        at_path_candidates: &'a [crate::chat::slash_types::AtPathCandidate],
        current_provider: &'a str,
    ) -> crate::chat::tui::SlashMenuSources<'a> {
        crate::chat::tui::SlashMenuSources {
            live_sessions,
            saved_sessions,
            provider_model_catalog,
            at_path_candidates,
            current_provider,
        }
    }

    /// 构造当前状态对应的 [`UiSnapshot`].
    ///
    /// Arc 字段（`conversation_lines`）让相邻未变 ui 的两次快照真正共享底层 Vec：
    /// `cached_lines_arc` 记录上次构造的 Arc，reduce_tracked 在 dirty=true 时清缓存，
    /// build_ui_snapshot 在缓存命中时 `Arc::clone` 复用（refcount 增量 + 0 拷贝）。
    ///
    /// `revision` 由调用方维护单调递增。
    #[cfg(feature = "terminal-tui")]
    #[must_use]
    #[allow(dead_code)]
    pub fn build_ui_snapshot(&mut self, revision: u64) -> UiSnapshot {
        let lines = if let Some(ref cached) = self.cached_lines_arc {
            Arc::clone(cached)
        } else {
            let arc = Arc::new(self.ui.conversation_lines.clone());
            self.cached_lines_arc = Some(Arc::clone(&arc));
            arc
        };
        UiSnapshot {
            revision,
            provider: Arc::clone(&self.session.provider),
            model: Arc::clone(&self.session.model),
            chat_mode: self.ui.chat_mode,
            autonomy_level: self.ui.autonomy_level,
            session_title: Arc::from(self.session.title.as_str()),
            turn_count: self.ui.turn_count,
            ascii_fallback: self.ui.ascii_fallback,
            conversation_lines: lines,
            conversation_generation: self.ui.conversation_generation,
            streaming: self.stream.primary_streaming_draft().cloned(),
            visible_streaming_drafts: Arc::new(self.stream.visible_streaming_draft_views()),
            input: self.ui.input.clone(),
            sessions_status: Arc::from(self.ui.sessions_status.as_str()),
            sessions_entries: Arc::new(self.ui.sessions_entries.clone()),
            main_queue_status: self.ui.main_queue_status,
            provider_worker_status: self.ui.provider_worker_status.clone(),
            active_session_view: self.ui.active_session_view.clone(),
            pending_tool_approval: self.ui.pending_tool_approval.clone(),
            context_used_tokens: self.ui.context_used_tokens,
            context_window_tokens: self.ui.context_window_tokens,
            token_usage_summary: self.ui.token_usage_summary,
            focus: self.ui.focus,
            switcher: self.ui.switcher.clone(),
            strip_selection: self.ui.strip_selection,
            slash_menu: self.ui.slash_menu.clone(),
            saved_session_picker: self.ui.saved_session_picker.clone(),
        }
    }

    /// `reduce` + 显式 ui_dirty 信号（S4-A Commit 1 引入）.
    ///
    /// 决策（Codex S4-A 阶段 1 评分 8.1/10 采纳）:
    /// - **不用** action 白名单作为 dirty 判定来源（易漏新 Action）
    /// - 改为根据 Action 变体名 + reducer 内部对 `ui.conversation_lines / stream.draft
    ///   / ui.input` 的实际写入决定 dirty
    ///
    /// 实现说明（与规划版本的偏离）:
    /// - 规划要求把 `reduce` 改成 `(Vec<Effect>, bool)` 签名。但 PRX 现有
    ///   ~250 个 test caller 用 `let effects = state.reduce(...)` 直接拿
    ///   `Vec<Effect>`，全量 destructure 改造收益远低于风险。
    /// - 实际把 dirty 决策放在本 wrapper 内：top-level match Action 变体，
    ///   exhaustive 检查保证新增 Action 编译期可见漏写。`reduce` / `reduce_with_now`
    ///   签名保持不变。
    ///
    /// dispatcher 只调用 `reduce_tracked`（Commit 3 接线），test 仍可
    /// 自由用 `reduce`/`reduce_with_now`。
    #[cfg(feature = "terminal-tui")]
    #[allow(dead_code)]
    pub fn reduce_tracked(&mut self, action: Action) -> (Vec<Effect>, bool) {
        let dirty = ui_dirty_for(&action);
        let snap_before = self.snapshot_dirty_fields();
        let effects = self.reduce(action);
        let snap_after = self.snapshot_dirty_fields();
        let dirty_final = dirty || (snap_before != snap_after);
        if dirty_final {
            // ui 变化时清缓存，下次 build_ui_snapshot 重建 Arc<Vec<ConversationLine>>
            self.cached_lines_arc = None;
        }
        (effects, dirty_final)
    }

    /// `reduce_tracked` 用于 dirty 判定的运行时兜底：返回 ui.conversation_lines.len() /
    /// stream.draft.is_some() 等粒度指纹.
    ///
    /// 注：仅对**长度/计数级**变化敏感（如 push 一行 / draft None→Some），不对
    /// 内容字节级变化敏感（如 streaming chunk 累积）— streaming 的内容变化由
    /// 静态 whitelist `ui_dirty_for` 兜住（StreamChunkReceived → true）.
    #[cfg(feature = "terminal-tui")]
    fn snapshot_dirty_fields(&self) -> SnapshotDirtyFields {
        SnapshotDirtyFields {
            conversation_len: self.ui.conversation_lines.len(),
            conversation_generation: self.ui.conversation_generation,
            draft_versions: self.stream.versions_fingerprint(),
            input_lines: self.ui.input.lines.len(),
            context_used_tokens: self.ui.context_used_tokens,
            context_window_tokens: self.ui.context_window_tokens,
            slash_menu_open: self.ui.slash_menu.is_some(),
            slash_menu_selected: self.ui.slash_menu.as_ref().map(|menu| menu.selected),
            chat_mode: self.ui.chat_mode,
            autonomy_level: self.ui.autonomy_level,
            approval_visible: self.ui.pending_tool_approval.is_some(),
            focus: self.ui.focus,
            token_usage_summary: self.ui.token_usage_summary,
            main_queue_status: self.ui.main_queue_status,
        }
    }

    /// 非 terminal-tui feature 下 ui_dirty 始终 false（无 UI 渲染源）.
    #[cfg(not(feature = "terminal-tui"))]
    #[allow(dead_code)]
    pub fn reduce_tracked(&mut self, action: Action) -> (Vec<Effect>, bool) {
        let effects = self.reduce(action);
        (effects, false)
    }

    /// 纯 sync 状态机 — 根据 [`Action`] mutate self，返回需要主循环执行的 [`Effect`] 列表.
    ///
    /// 约束:
    /// - 无 `.await`，无 I/O，无 `spawn`
    /// - 所有 async 副作用通过 `Effect` 返回，由主循环 dispatch
    /// - 内部调用 `now_ms()` 读取墙钟（双击窗口判断），见 [`Self::reduce_with_now`]
    ///   暴露的纯参数化版本以便测试注入时间
    pub fn reduce(&mut self, action: Action) -> Vec<Effect> {
        let now = now_ms();
        self.reduce_with_now(action, now)
    }

    /// 与 [`Self::reduce`] 等价，但 `now_ms` 显式注入以便测试构造确定时间.
    pub fn reduce_with_now(&mut self, action: Action, now_ms: u64) -> Vec<Effect> {
        // S2.5 T2.5-2: 入口埋点 prx_chat_actions_total{action_kind=...}.
        crate::observability::chat_metrics::inc_action(action.kind());
        match action {
            // ── 输入路径 ──────────────────────────────────────────
            Action::KeyPressed(key) => self.reduce_key_pressed(key, now_ms),
            Action::PasteReceived(text) => self.reduce_paste_received(&text),
            Action::TerminalResized { w: _w, h: _h } => {
                // Step 2: 无尺寸缓存，仅请求重绘（ratatui 自动适配）
                vec![Effect::RequestRedraw]
            }
            Action::InputSubmitted(text) => self.reduce_input_submitted(text),
            Action::InputReplaced(text) => self.reduce_input_replaced(&text),
            Action::HistoryNavigated(dir) => self.reduce_history_navigated(dir),
            Action::InputCancelled => self.reduce_input_cancelled(),

            // ── 槽命令 ────────────────────────────────────────────
            Action::SlashCommandIssued { cmd: _cmd, args: _args } => {
                // Step 4: 分发到 commands 模块处理
                vec![]
            }
            Action::ModeChanged(mode) => {
                self.session.mode = mode;
                self.ui.chat_mode = mode;
                vec![Effect::RequestRedraw]
            }
            Action::ModelChanged { model } => {
                // BUG-07: /model <name> 在线切换。更新 session.model 让 status bar
                // 立刻显示新 model；后续 LLM turn 真切 model 由主循环写 EffectDeps
                // 热替换 slot 完成（reducer 不持有 provider，故只负责 UI 账本）。
                self.session.model = Arc::from(model.as_str());
                vec![Effect::RequestRedraw]
            }
            Action::ProviderChanged { provider, model } => {
                // Bug #3: /provider <name> [model] 在线切换。更新 session.provider 让
                // status bar / snapshot 立刻反映新 provider；若同时换了 model 也一并写
                // session.model。后续 LLM turn 真切 provider 实例由主循环写 ProviderSlot
                // 热替换 slot 完成（reducer 不持有 provider，只负责 UI/session 账本）。
                self.session.provider = Arc::from(provider.as_str());
                if let Some(model) = model {
                    self.session.model = Arc::from(model.as_str());
                }
                vec![Effect::RequestRedraw]
            }
            Action::HistoryCleared => self.reduce_history_cleared(),
            Action::HistoryClearedWithNotice { notice } => self.reduce_history_cleared_with_notice(notice),
            Action::HistoryCompacted { reason } => self.reduce_history_compacted(reason),
            Action::HistoryCompactionPatchApplied {
                reason,
                patch,
                compaction_config,
            } => self.reduce_history_compaction_patch_applied(reason, patch, &compaction_config),

            // ── LLM 流式 (Step 3) ─────────────────────────────────
            Action::TurnStarted { draft_id, cancel } => self.reduce_turn_started(draft_id, cancel),
            Action::StartLLMTurn {
                provider_turn_task_id,
                provider_turn_sequence,
                draft_id,
                history,
                compaction_config,
                cancel,
                turn_spawn_ctx,
                turn_message_send_ctx,
            } => self.reduce_start_llm_turn(
                provider_turn_task_id,
                provider_turn_sequence,
                draft_id,
                history,
                compaction_config,
                cancel,
                turn_spawn_ctx,
                turn_message_send_ctx,
            ),
            Action::StreamChunkReceived {
                draft_id,
                delta,
                version,
            } => self.reduce_stream_chunk_received(&draft_id, &delta, version),
            Action::StreamUsageMetered { .. } => vec![],
            Action::StreamCompleted {
                draft_id,
                final_text,
                reasoning,
            } => self.reduce_stream_completed(&draft_id, final_text, reasoning),
            Action::ProviderTurnReadyForCommit { .. } => vec![],
            Action::StreamFailed {
                draft_id,
                err,
                retryable,
            } => self.reduce_stream_failed(&draft_id, err, retryable),
            Action::StreamCancelled { draft_id } => self.reduce_stream_cancelled(&draft_id),

            // ── 工具事件 (Step 3) ─────────────────────────────────
            Action::ToolStarted {
                task_id,
                sequence,
                tool_call_id,
                name,
                args,
            } => self.reduce_tool_started(task_id, sequence, tool_call_id, name, args),
            Action::ToolFinished {
                task_id,
                sequence,
                tool_call_id,
                name,
                success,
                duration_ms,
                result,
            } => self.reduce_tool_finished(task_id, sequence, tool_call_id, name, success, duration_ms, result),
            Action::ToolProgress { iteration, max } => self.reduce_tool_progress(iteration, max),
            Action::ToolApprovalRequested {
                task_id,
                tool_id,
                name,
                args,
            } => self.reduce_tool_approval_requested(task_id, tool_id, name, args),
            Action::ToolApprovalReceived { tool_id, approved } => {
                self.reduce_tool_approval_received(&tool_id, approved)
            }
            Action::ToolApprovalCleared => self.reduce_tool_approval_cleared(),
            Action::StreamRetryAttempt { attempt, reason } => self.reduce_stream_retry_attempt(attempt, &reason),

            // ── 会话 ──────────────────────────────────────────────
            Action::SessionLoaded(session) => self.reduce_session_loaded(session),
            Action::SessionSaved { id } => self.reduce_session_saved(id),
            Action::SessionSwitched { id } => self.reduce_session_switched(id),
            Action::RecordUserTurn(content) => self.reduce_record_user_turn(content),
            Action::RecordAssistantTurn { task_id, content } => self.reduce_record_assistant_turn(task_id, content),
            Action::RecordSystemMessage { content } => self.reduce_record_system_message(content),
            Action::SetLeadingSystemPrompt { content } => self.reduce_set_leading_system_prompt(content),

            // ── UI 折叠/展开 ────────────────────────────────────
            Action::ToolCardFoldToggled => self.reduce_tool_card_fold_toggled(),
            Action::ReasoningFoldToggled => self.reduce_reasoning_fold_toggled(),
            Action::RedrawRequested => vec![Effect::RequestRedraw],
            Action::SystemMessageAdded { text } => self.reduce_system_message_added(text),
            Action::UserMessageEchoed(text) => self.reduce_user_message_echoed(text),
            Action::SessionsStatusUpdated { summary } => self.reduce_sessions_status_updated(summary),
            Action::SessionsEntriesUpdated { entries } => self.reduce_sessions_entries_updated(entries),
            Action::MainQueueStatusUpdated { status } => self.reduce_main_queue_status_updated(status),
            Action::ProviderWorkerStatusUpdated { status } => self.reduce_provider_worker_status_updated(status),
            Action::SlashMenuSourcesUpdated {
                saved_sessions,
                provider_model_catalog,
            } => self.reduce_slash_menu_sources_updated(saved_sessions, provider_model_catalog),
            Action::AtPathCandidatesUpdated { candidates } => self.reduce_at_path_candidates_updated(candidates),
            Action::ActiveSessionViewUpdated { view } => self.reduce_active_session_view_updated(view),
            Action::ContextWindowUpdated {
                used_context_tokens,
                max_context_tokens,
            } => self.reduce_context_window_updated(used_context_tokens, max_context_tokens),
            Action::ProviderUsageRecorded {
                task_id,
                usage_kind,
                record,
            } => self.reduce_provider_usage_recorded(task_id, usage_kind, record),
            Action::BackgroundSessionRecorded { summary } => self.reduce_background_session_recorded(summary),
            Action::SessionFocusChanged { focus } => self.reduce_session_focus_changed(focus),
            Action::SwitcherOpened { entries } => self.reduce_switcher_opened(entries),
            Action::SwitcherMoved { selected } => self.reduce_switcher_moved(selected),
            Action::SwitcherClosed => self.reduce_switcher_closed(),
            Action::StripSelectionChanged { selected } => self.reduce_strip_selection_changed(selected),
            Action::SavedSessionPickerOpened { entries } => self.reduce_saved_session_picker_opened(entries),
            Action::SavedSessionPickerMoved { selected } => self.reduce_saved_session_picker_moved(selected),
            Action::SavedSessionPickerClosed => self.reduce_saved_session_picker_closed(),

            // ── 退出 ──────────────────────────────────────────────
            Action::CancelRequested => self.reduce_cancel_requested(),
            Action::CancelProviderTurn { task_id } => self.reduce_cancel_provider_turn(task_id),
            Action::ShutdownRequested => self.reduce_shutdown_requested(),
            Action::ForceQuit => vec![Effect::Quit],
        }
    }

    // ── 输入路径子函数（Step 2） ────────────────────────────────────────────────

    /// 处理 `KeyPressed`：按键分发到 input buffer / 全局快捷键 / 退出语义.
    ///
    /// 此函数本质上是 `tui::dispatch_global_key` 的 reducer 版本，但作用对象是
    /// `UiState.input` 而非 `TuiState`。返回 Effect 序列（典型只有 RequestRedraw）。
    /// 实际向 channel 投递 user message / 触发 cancel 等仍由主循环根据 Effect 执行。
    #[cfg(feature = "terminal-tui")]
    fn reduce_key_pressed(&mut self, key: crossterm::event::KeyEvent, now_ms: u64) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyModifiers};

        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            let prev = self.ui.last_ctrlc_ms;
            self.ui.last_ctrlc_ms = now_ms;
            if prev != 0 && now_ms.saturating_sub(prev) < DOUBLE_CTRLC_WINDOW_MS {
                return vec![Effect::Quit];
            }
            return self.reduce_cancel_requested();
        }
        if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL && self.ui.input.is_empty() {
            return vec![Effect::Quit];
        }
        if self.ui.saved_session_picker.is_some() {
            return self.reduce_saved_session_picker_key_pressed(key);
        }
        if self.ui.switcher.is_some() {
            if key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return self.reduce_switcher_closed();
            }
            return vec![Effect::RequestRedraw];
        }
        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE && self.ui.strip_selection.take().is_some() {
            return vec![Effect::RequestRedraw];
        }
        if self.ui.pending_tool_approval.is_some()
            || matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval)
        {
            return self.reduce_approval_key_pressed(key);
        }

        if self.ui.slash_menu.is_some() {
            let sources = Self::slash_menu_sources_from(
                &self.ui.sessions_entries,
                &self.ui.saved_sessions_cache,
                &self.ui.provider_model_catalog,
                &self.ui.at_path_candidates,
                self.session.provider.as_ref(),
            );
            let dispatch = crate::chat::tui::dispatch_slash_menu_key_with_sources(
                &mut self.ui.input,
                &mut self.ui.slash_menu,
                key,
                sources,
            );
            return match dispatch {
                crate::chat::tui::KeyDispatch::Submitted(text) => self.reduce_input_submitted(text),
                crate::chat::tui::KeyDispatch::Cancelled => self.reduce_input_cancelled(),
                crate::chat::tui::KeyDispatch::Ignored => Vec::new(),
                _ => vec![Effect::RequestRedraw],
            };
        }

        if key.modifiers == KeyModifiers::ALT {
            let direction = match key.code {
                KeyCode::Left | KeyCode::Up => Some(crate::chat::sessions::SessionDirection::Previous),
                KeyCode::Right | KeyCode::Down => Some(crate::chat::sessions::SessionDirection::Next),
                _ => None,
            };
            if let Some(direction) = direction {
                self.ui.strip_selection = crate::chat::tui::move_strip_selection(
                    &self.ui.sessions_entries,
                    self.ui.strip_selection,
                    self.ui.focus,
                    direction,
                );
                return vec![Effect::RequestRedraw];
            }
            if key.code == KeyCode::Enter
                && let Some(selected) = self.ui.strip_selection
            {
                if crate::chat::tui::selected_strip_entry(&self.ui.sessions_entries, Some(selected)).is_some() {
                    return vec![
                        Effect::LogTrace {
                            level: tracing::Level::DEBUG,
                            msg: format!("strip_alt_enter_attach seq={selected}"),
                        },
                        Effect::RequestRedraw,
                    ];
                }
                self.ui.strip_selection = None;
                self.ui
                    .conversation_lines
                    .push(crate::chat::tui::ConversationLine::System {
                        content: "session gone".to_string(),
                    });
                return vec![Effect::RequestRedraw];
            }
        }

        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE && self.control.generating {
            return self.reduce_cancel_requested();
        }

        // Tab → 折叠/展开最近的可折叠卡片（Reasoning 或 ToolResult，取更靠后的）。
        // BUG-01: 旧实现只切 ToolResult，导致 thinking/Reasoning 卡按 Tab 永不展开
        // （折叠提示却写着 "press Tab to expand"）。与 tui::toggle_last_foldable_card
        // 的统一语义对齐。
        if key.code == KeyCode::Tab && key.modifiers == KeyModifiers::NONE && self.ui.input.is_empty() {
            return self.reduce_foldable_card_toggled();
        }
        // Ctrl+R → reverse-search submitted input history. Tab is the sole
        // fold binding after P6b2.
        if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL {
            let _ = self.ui.input.begin_or_cycle_reverse_search();
            return vec![Effect::RequestRedraw];
        }
        // Ctrl+L → 清屏（请求重绘即可，host 终端清屏由 effect 执行器决定）
        if key.code == KeyCode::Char('l') && key.modifiers == KeyModifiers::CONTROL {
            return vec![Effect::RequestRedraw];
        }
        // Ctrl+D → 空 buffer 退出 / 非空 forward-delete（委托 handle_key）
        if key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL {
            // 非空 buffer 转发为 Delete
            let synthetic = crossterm::event::KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
            let _ = self.ui.input.handle_key(synthetic);
            let sources = Self::slash_menu_sources_from(
                &self.ui.sessions_entries,
                &self.ui.saved_sessions_cache,
                &self.ui.provider_model_catalog,
                &self.ui.at_path_candidates,
                self.session.provider.as_ref(),
            );
            crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
            return vec![Effect::RequestRedraw];
        }
        // 其他键 → 转发到 input buffer，根据 InputOutcome 派生后续 Action 自递归
        match self.ui.input.handle_key(key) {
            crate::chat::tui::InputOutcome::Submitted(text) => {
                self.ui.slash_menu = None;
                // 用 reduce_with_now 重入以保持单一处理路径
                self.reduce_input_submitted(text)
            }
            crate::chat::tui::InputOutcome::Cancelled => {
                self.ui.slash_menu = None;
                self.reduce_input_cancelled()
            }
            crate::chat::tui::InputOutcome::Consumed | crate::chat::tui::InputOutcome::Unhandled => {
                let sources = Self::slash_menu_sources_from(
                    &self.ui.sessions_entries,
                    &self.ui.saved_sessions_cache,
                    &self.ui.provider_model_catalog,
                    &self.ui.at_path_candidates,
                    self.session.provider.as_ref(),
                );
                crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
                vec![Effect::RequestRedraw]
            }
            crate::chat::tui::InputOutcome::Ignored => Vec::new(),
        }
    }

    #[cfg(feature = "terminal-tui")]
    fn reduce_approval_key_pressed(&mut self, key: crossterm::event::KeyEvent) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyModifiers};
        let Some(pending) = self.ui.pending_tool_approval.clone() else {
            if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
                if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
                    self.ui.focus = crate::chat::sessions::FocusTarget::Main;
                }
                return vec![Effect::RequestRedraw];
            }
            return vec![Effect::RequestRedraw];
        };
        if key.modifiers != KeyModifiers::NONE {
            return vec![Effect::RequestRedraw];
        }
        let approved = match key.code {
            KeyCode::Char('y' | 'Y') => Some(true),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
            _ => None,
        };
        let Some(approved) = approved else {
            return vec![Effect::RequestRedraw];
        };
        self.ui.pending_tool_approval = None;
        if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
            self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        }
        vec![
            Effect::ResolveApproval {
                tool_id: pending.tool_id,
                approved,
            },
            Effect::RequestRedraw,
        ]
    }

    /// 非 terminal-tui feature 下的占位（KeyEvent 仅在 crossterm 可用时存在）
    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut, clippy::missing_const_for_fn)]
    fn reduce_key_pressed(&mut self, _key: crossterm::event::KeyEvent, _now_ms: u64) -> Vec<Effect> {
        let _ = &self.ui;
        vec![]
    }

    /// 处理括号粘贴：将文本插入到 input buffer.
    #[cfg(feature = "terminal-tui")]
    fn reduce_paste_received(&mut self, text: &str) -> Vec<Effect> {
        if self.ui.pending_tool_approval.is_some()
            || matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval)
        {
            return vec![Effect::RequestRedraw];
        }
        self.ui.input.paste(text);
        let sources = Self::slash_menu_sources_from(
            &self.ui.sessions_entries,
            &self.ui.saved_sessions_cache,
            &self.ui.provider_model_catalog,
            &self.ui.at_path_candidates,
            self.session.provider.as_ref(),
        );
        crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_paste_received(&mut self, _text: &str) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// 处理用户提交 — 仅做 UI 侧记账（turn_count + last_submitted）.
    ///
    /// Step 2 不触发 LLM（Step 3 才追加 `Effect::StartTurn`）。
    /// `LogTrace` 用于双写期对账。
    fn reduce_input_submitted(&mut self, text: String) -> Vec<Effect> {
        self.ui.slash_menu = None;
        self.ui.turn_count = self.ui.turn_count.saturating_add(1);
        let log_msg = format!("input_submitted len={}", text.chars().count());
        self.ui.last_submitted = Some(text);
        self.ui.input.clear();
        // Legacy main-turn entry only clears the Primary tool bucket; keyed
        // worker buckets must survive until their own terminal event.
        self.control.clear_tool_buffer(ToolTaskKey::Primary);
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: log_msg,
            },
            Effect::RequestRedraw,
        ]
    }

    #[cfg(feature = "terminal-tui")]
    fn reduce_input_replaced(&mut self, text: &str) -> Vec<Effect> {
        self.ui.input.set_text(text);
        self.ui.input.clear_navigation_state();
        let sources = Self::slash_menu_sources_from(
            &self.ui.sessions_entries,
            &self.ui.saved_sessions_cache,
            &self.ui.provider_model_catalog,
            &self.ui.at_path_candidates,
            self.session.provider.as_ref(),
        );
        crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_input_replaced(&mut self, _text: &str) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// 处理 Up/Down 历史导航.
    #[cfg(feature = "terminal-tui")]
    fn reduce_history_navigated(&mut self, dir: HistoryDir) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = match dir {
            HistoryDir::Up => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            HistoryDir::Down => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        };
        let _ = self.ui.input.handle_key(key);
        let sources = Self::slash_menu_sources_from(
            &self.ui.sessions_entries,
            &self.ui.saved_sessions_cache,
            &self.ui.provider_model_catalog,
            &self.ui.at_path_candidates,
            self.session.provider.as_ref(),
        );
        crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_history_navigated(&mut self, _dir: HistoryDir) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// 处理 Esc — 清空 input buffer.
    #[cfg(feature = "terminal-tui")]
    fn reduce_input_cancelled(&mut self) -> Vec<Effect> {
        self.ui.slash_menu = None;
        self.ui.input.clear();
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_input_cancelled(&mut self) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// 处理 Tab — 折叠/展开最近的可折叠卡片（Reasoning 或 ToolResult）。
    ///
    /// BUG-01: 取 `conversation_lines` 中**最靠后**的 Reasoning 或 ToolResult，
    /// 翻转其 `folded`。与 `tui::TuiState::toggle_last_foldable_card` 同语义，
    /// 保证 Pure(reducer/snapshot) 路径下 Tab 能展开 thinking 卡片。
    #[cfg(feature = "terminal-tui")]
    fn reduce_foldable_card_toggled(&mut self) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let mut toggled = false;
        for line in self.ui.conversation_lines.iter_mut().rev() {
            match line {
                ConversationLine::Reasoning { folded, .. } | ConversationLine::ToolResult { folded, .. } => {
                    *folded = !*folded;
                    toggled = true;
                    break;
                }
                _ => {}
            }
        }
        // A fold toggle must still mark the conversation as changed so the
        // snapshot/repaint path observes the new fold state. Only bump when a
        // card was toggled to avoid spurious redraw work on no-op key presses.
        if toggled {
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        }
        vec![Effect::RequestRedraw]
    }

    /// 处理 Tab — 折叠/展开最近 ToolResult.
    #[cfg(feature = "terminal-tui")]
    fn reduce_tool_card_fold_toggled(&mut self) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let mut toggled = false;
        for line in self.ui.conversation_lines.iter_mut().rev() {
            if let ConversationLine::ToolResult { folded, .. } = line {
                *folded = !*folded;
                toggled = true;
                break;
            }
        }
        // BUG-01 round-2 fix: re-emit scrollback so the new fold state is visible
        // (see `reduce_foldable_card_toggled` for the full rationale).
        if toggled {
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_tool_card_fold_toggled(&mut self) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// 处理 Ctrl+R — 折叠/展开最近 Reasoning.
    #[cfg(feature = "terminal-tui")]
    fn reduce_reasoning_fold_toggled(&mut self) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let mut toggled = false;
        for line in self.ui.conversation_lines.iter_mut().rev() {
            if let ConversationLine::Reasoning { folded, .. } = line {
                *folded = !*folded;
                toggled = true;
                break;
            }
        }
        // BUG-01 round-2 fix: re-emit scrollback so the new fold state is visible
        // (see `reduce_foldable_card_toggled` for the full rationale).
        if toggled {
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_reasoning_fold_toggled(&mut self) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    // ── 流式 / 工具子函数（Step 3） ──────────────────────────────────────────
    //
    // P3-5 版本号机制完整下沉：reducer 内的 `StreamState::draft` 是
    // 单一防护源。版本号比对（strict-monotonic）由 `reduce_stream_chunk_received`
    // 实现，规则：
    //   1. 无 draft（已 finalize）→ 丢弃
    //   2. draft_id 不匹配（跨 turn stale）→ 丢弃
    //   3. version <= 当前 draft.version → 丢弃（含相等，strict-monotonic）
    //   4. 否则：累积 delta + 更新 version + RequestRedraw
    //
    // 旧 `DraftVersionTracker`（HashMap-based、Mutex-guarded）作为过度防御
    // 自 Step 3 起从 `chat::mod::draft_updater` 任务中撤除（单线程 mpsc 自然
    // FIFO，counter 足够保证 monotonic）。Reducer 接管后版本号机制只在一处，
    // 杜绝双写期竞争。

    fn visible_draft_sequence(task_id: Option<crate::chat::turn_scheduler::TurnTaskId>, sequence: Option<u64>) -> u64 {
        sequence
            .or_else(|| task_id.map(crate::chat::turn_scheduler::TurnTaskId::get))
            .unwrap_or(0)
    }

    fn prompt_preview_from_history(history: &[ChatMessage]) -> String {
        history
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map_or_else(String::new, |message| truncate_with_ellipsis(&message.content, 96))
    }

    fn insert_visible_streaming_draft(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
        draft_id: String,
        prompt_preview: String,
    ) {
        self.stream.insert_visible_draft(StreamingTurnDraft {
            task_id,
            sequence: Self::visible_draft_sequence(task_id, sequence),
            prompt_preview,
            draft: StreamingDraft {
                draft_id,
                accumulated: String::new(),
                version: 0,
            },
        });
    }

    #[must_use]
    fn sequence_for_tool_key(&self, key: ToolTaskKey) -> Option<u64> {
        let ToolTaskKey::Task(task_id) = key else {
            return None;
        };
        self.stream
            .visible_drafts
            .iter()
            .find(|draft| draft.task_id == Some(task_id))
            .map(|draft| draft.sequence)
    }

    /// `Action::TurnStarted` — 初始化 streaming draft + 注册取消令牌.
    #[cfg(feature = "terminal-tui")]
    fn reduce_turn_started(&mut self, draft_id: String, cancel: CancellationToken) -> Vec<Effect> {
        self.insert_visible_streaming_draft(None, None, draft_id.clone(), String::new());
        self.control.clear_tool_buffer(ToolTaskKey::Primary);
        self.control.register_turn_cancel(ToolTaskKey::Primary, cancel);
        self.control.generating = true;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("turn_started draft_id={draft_id}"),
            },
            Effect::RequestRedraw,
        ]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_turn_started(&mut self, draft_id: String, cancel: CancellationToken) -> Vec<Effect> {
        self.insert_visible_streaming_draft(None, None, draft_id.clone(), String::new());
        self.control.clear_tool_buffer(ToolTaskKey::Primary);
        self.control.register_turn_cancel(ToolTaskKey::Primary, cancel);
        self.control.generating = true;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("turn_started draft_id={draft_id}"),
            },
            Effect::RequestRedraw,
        ]
    }

    /// Step 5a-3 Phase A — `Action::StartLLMTurn`：发起 LLM 流式 turn.
    ///
    /// 行为:
    /// 1. 状态变更与 [`Self::reduce_turn_started`] 一致（初始化 draft、注册 cancel、置 generating）
    /// 2. **额外**发射 `Effect::StartTurn { draft_id, history, cancel }`，由 EffectExecutor
    ///    在 real-deps 模式下 spawn 子任务真接 `provider.stream_chat_with_history`
    ///
    /// 与 `TurnStarted` 的核心区别：携带 history 快照让 reducer 能驱动真 LLM 流式。
    /// Phase A 阶段 chat::run 主循环旧路径并未切换；本 Action 仅供 Phase B+ 主循环
    /// 切换、或单元测试验证 reducer → Effect → EffectExecutor 闭环时使用。
    #[cfg(feature = "terminal-tui")]
    fn reduce_start_llm_turn(
        &mut self,
        provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        provider_turn_sequence: Option<u64>,
        draft_id: String,
        history: Vec<crate::providers::ChatMessage>,
        compaction_config: Option<crate::config::AgentCompactionConfig>,
        cancel: CancellationToken,
        turn_spawn_ctx: Option<crate::tools::sessions_spawn::SpawnExecutionContext>,
        turn_message_send_ctx: Option<crate::tools::message_send::MessageSendExecutionContext>,
    ) -> Vec<Effect> {
        self.insert_visible_streaming_draft(
            provider_turn_task_id,
            provider_turn_sequence,
            draft_id.clone(),
            Self::prompt_preview_from_history(&history),
        );
        self.control
            .clear_tool_buffer(ToolTaskKey::from_task_id(provider_turn_task_id));
        self.control
            .register_turn_cancel(ToolTaskKey::from_task_id(provider_turn_task_id), cancel.clone());
        self.control.generating = true;
        // BUG-09: capture the current chat mode so the driver can enforce plan
        // mode's read-only contract on write/shell/git tools.
        let chat_mode = self.session.mode;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("start_llm_turn draft_id={draft_id} history_len={}", history.len()),
            },
            Effect::StartTurn {
                provider_turn_task_id,
                draft_id,
                history,
                compaction_guard_history: Some(self.session.history.clone()),
                compaction_config,
                cancel,
                chat_mode,
                turn_spawn_ctx,
                turn_message_send_ctx,
            },
            Effect::RequestRedraw,
        ]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_start_llm_turn(
        &mut self,
        provider_turn_task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        provider_turn_sequence: Option<u64>,
        draft_id: String,
        history: Vec<crate::providers::ChatMessage>,
        compaction_config: Option<crate::config::AgentCompactionConfig>,
        cancel: CancellationToken,
        turn_spawn_ctx: Option<crate::tools::sessions_spawn::SpawnExecutionContext>,
        turn_message_send_ctx: Option<crate::tools::message_send::MessageSendExecutionContext>,
    ) -> Vec<Effect> {
        self.insert_visible_streaming_draft(
            provider_turn_task_id,
            provider_turn_sequence,
            draft_id.clone(),
            Self::prompt_preview_from_history(&history),
        );
        self.control
            .clear_tool_buffer(ToolTaskKey::from_task_id(provider_turn_task_id));
        self.control
            .register_turn_cancel(ToolTaskKey::from_task_id(provider_turn_task_id), cancel.clone());
        self.control.generating = true;
        // BUG-09: capture the current chat mode so the driver can enforce plan
        // mode's read-only contract on write/shell/git tools.
        let chat_mode = self.session.mode;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("start_llm_turn draft_id={draft_id} history_len={}", history.len()),
            },
            Effect::StartTurn {
                provider_turn_task_id,
                draft_id,
                history,
                compaction_guard_history: Some(self.session.history.clone()),
                compaction_config,
                cancel,
                chat_mode,
                turn_spawn_ctx,
                turn_message_send_ctx,
            },
            Effect::RequestRedraw,
        ]
    }

    /// `Action::StreamChunkReceived` — 版本号防护 + 累积 delta.
    ///
    /// 返回值：
    /// - 接受时 → `[RequestRedraw]`
    /// - 丢弃时 → `[]`（静默；调用方可通过比较 draft.version 前后是否变化判断）
    #[cfg(feature = "terminal-tui")]
    fn reduce_stream_chunk_received(&mut self, draft_id: &str, delta: &str, version: u64) -> Vec<Effect> {
        let Some(turn) = self.stream.visible_draft_mut(draft_id) else {
            // 已 finalize — chunk 视为 stale，丢弃
            return vec![];
        };
        let draft = &mut turn.draft;
        if version <= draft.version {
            // 严格单调：等于或更小都视为乱序/重复，丢弃
            return vec![];
        }
        draft.accumulated.push_str(delta);
        draft.version = version;
        self.refresh_provider_worker_view_if_focused();
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_stream_chunk_received(&mut self, draft_id: &str, delta: &str, version: u64) -> Vec<Effect> {
        let Some(turn) = self.stream.visible_draft_mut(draft_id) else {
            return vec![];
        };
        let draft = &mut turn.draft;
        if version <= draft.version {
            return vec![];
        }
        draft.accumulated.push_str(delta);
        draft.version = version;
        vec![Effect::RequestRedraw]
    }

    /// `Action::StreamCompleted` — 清除 draft + push assistant message + 通知钩子 + **持久化会话**.
    ///
    /// T3-3-c: Effect 序列末尾追加 [`Effect::SaveSession`]，让 reducer 在每轮完成时
    /// 触发会话快照写入；这把 Pure 模式下原本由 legacy `chat_session.add_*_turn`
    /// + `save_session(...)` 完成的持久化收敛到 reducer 单源。
    ///
    /// Effect 顺序保证（执行器按 Vec 顺序消费）：
    ///
    /// - `[0]` NotifyHook(TurnComplete) — webhook / observer 先知道本轮完成
    /// - `[1]` SaveSession(snapshot)    — 持久化 turns（dual_write_guard 防双写）
    /// - `[2]` RequestRedraw            — UI 刷新放最后
    ///
    /// **重要**：快照基于 reducer 自己的 `session.turns`（由 `RecordAssistantTurn` 写入），
    /// 不是 legacy `chat_session` 副本。Pure 模式下两者本就同步，legacy 副本被 T3-3-c
    /// 守卫跳过；Off / Both / Redux 模式 dual_write_guard 抑制重复保存。
    #[cfg(feature = "terminal-tui")]
    fn reduce_stream_completed(&mut self, draft_id: &str, final_text: String, reasoning: String) -> Vec<Effect> {
        use crate::chat::tui::ConversationLine;
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        self.remove_pending_tool_cards(tool_key);
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        if !final_text.is_empty() {
            self.ui.conversation_lines.push(ConversationLine::Assistant {
                content: final_text.clone(),
            });
        }
        if !reasoning.trim().is_empty() {
            let char_count = reasoning.chars().count();
            self.ui.conversation_lines.push(ConversationLine::Reasoning {
                content: reasoning,
                char_count,
                folded: true,
            });
        }
        self.refresh_provider_worker_view_if_focused();
        let chars = final_text.chars().count();
        let effects = vec![
            Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({
                    "mode": "chat",
                    "response_chars": chars,
                }),
            },
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::RequestRedraw,
        ];
        // Fallback for drivers that miss RecordAssistantTurn: only the completed
        // task's buffer is discarded.
        self.control.clear_tool_buffer(tool_key);
        effects
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_stream_completed(&mut self, draft_id: &str, final_text: String, reasoning: String) -> Vec<Effect> {
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        self.remove_pending_tool_cards(tool_key);
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        if !final_text.is_empty() {
            self.ui.conversation_lines.push(final_text.clone());
        }
        if !reasoning.trim().is_empty() {
            self.ui.conversation_lines.push(reasoning);
        }
        let chars = final_text.chars().count();
        let effects = vec![
            Effect::NotifyHook {
                event: HookEvent::TurnComplete,
                payload: serde_json::json!({
                    "mode": "chat",
                    "response_chars": chars,
                }),
            },
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::RequestRedraw,
        ];
        self.control.clear_tool_buffer(tool_key);
        effects
    }

    /// `Action::StreamFailed` — 清除 draft + LogTrace + NotifyHook(Error).
    ///
    /// Phase F：与旧路径在 chat::run 主循环里 `hooks.emit(HookEvent::Error, payload_error(...))`
    /// 的语义保持一致 — failed turn 必须触发 Error hook，否则外部审计 / webhook 会漏报。
    /// retryable 字段由 EffectExecutor 上层（chat::run 主循环重试逻辑）观察决定是否重发；
    /// hook 一律触发，因为对外可见的"本轮失败"是确定事件。
    fn reduce_stream_failed(&mut self, draft_id: &str, err: String, retryable: bool) -> Vec<Effect> {
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        let orphan_user_removed = if no_visible_drafts {
            self.rollback_trailing_answerless_user_turn()
        } else {
            false
        };
        self.finalize_pending_tool_cards(tool_key, false, Some("turn failed before tool finish event"));
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        self.control.clear_tool_buffer(tool_key);
        #[cfg(feature = "terminal-tui")]
        self.ui
            .conversation_lines
            .push(crate::chat::tui::ConversationLine::System {
                content: format!("Error: {err}"),
            });
        vec![
            Effect::LogTrace {
                level: tracing::Level::WARN,
                msg: format!(
                    "stream_failed draft_id={draft_id} retryable={retryable} orphan_user_removed={orphan_user_removed} err={err}"
                ),
            },
            Effect::NotifyHook {
                event: HookEvent::Error,
                payload: serde_json::json!({
                    "component": "chat-turn",
                    "message": err,
                    "retryable": retryable,
                    "draft_id": draft_id,
                }),
            },
            Effect::RequestRedraw,
        ]
    }

    /// `Action::StreamCancelled` — 用户主动取消，仅清除 draft.
    fn reduce_stream_cancelled(&mut self, draft_id: &str) -> Vec<Effect> {
        let Some(removed_draft) = self.stream.remove_visible_draft(draft_id) else {
            return vec![];
        };
        let tool_key = ToolTaskKey::from_task_id(removed_draft.task_id);
        self.control.remove_turn_cancel(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        self.finalize_pending_tool_cards(tool_key, false, Some("turn cancelled before tool finish event"));
        if no_visible_drafts {
            self.rollback_trailing_answerless_user_turn();
        }
        if no_visible_drafts && !self.control.has_task_turn_cancels() {
            self.control.active_cancel = None;
            self.control.generating = false;
        }
        self.control.clear_tool_buffer(tool_key);
        vec![Effect::RequestRedraw]
    }

    fn rollback_trailing_answerless_user_turn(&mut self) -> bool {
        let Some(last_turn) = self.session.turns.last() else {
            return false;
        };
        if last_turn.role != "user" {
            return false;
        }
        let content = last_turn.content.clone();
        let title_from_user = crate::chat::session::truncate_title(&content);
        self.session.turns.pop();
        if self
            .session
            .history
            .last()
            .is_some_and(|message| message.role == "user" && message.content == content)
        {
            self.session.history.pop();
        }
        if self.session.turns.is_empty() && self.session.title == title_from_user {
            self.session.title.clear();
        }
        true
    }

    #[cfg(feature = "terminal-tui")]
    fn remove_pending_tool_cards(&mut self, key: ToolTaskKey) {
        use crate::chat::tui::{ConversationLine, ToolStatus};

        let mut indices = self
            .control
            .tool_buffers
            .get_mut(&key)
            .map(|buffer| buffer.pending_tool_cards.drain(..).collect::<Vec<_>>())
            .unwrap_or_default();
        indices.sort_unstable_by(|a, b| b.cmp(a));
        indices.dedup();
        for idx in indices {
            if matches!(
                self.ui.conversation_lines.get(idx),
                Some(ConversationLine::ToolResult {
                    status: ToolStatus::Running,
                    ..
                })
            ) {
                self.ui.conversation_lines.remove(idx);
            }
        }
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn remove_pending_tool_cards(&mut self, key: ToolTaskKey) {
        if let Some(buffer) = self.control.tool_buffers.get_mut(&key) {
            buffer.pending_tool_cards.clear();
        }
    }

    #[cfg(feature = "terminal-tui")]
    fn finalize_pending_tool_cards(&mut self, key: ToolTaskKey, success: bool, fallback_result: Option<&'static str>) {
        use crate::chat::tui::{ConversationLine, ToolStatus};

        let Some(buffer) = self.control.tool_buffers.get_mut(&key) else {
            return;
        };
        for idx in buffer.pending_tool_cards.drain(..) {
            let Some(ConversationLine::ToolResult {
                status,
                result,
                elapsed_ms,
                ..
            }) = self.ui.conversation_lines.get_mut(idx)
            else {
                continue;
            };
            if *status != ToolStatus::Running {
                continue;
            }
            *status = if success { ToolStatus::Done } else { ToolStatus::Error };
            if result.is_none()
                && let Some(fallback_result) = fallback_result
            {
                *result = Some(fallback_result.to_string());
            }
            if elapsed_ms.is_none() {
                *elapsed_ms = Some(0);
            }
        }
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn finalize_pending_tool_cards(
        &mut self,
        key: ToolTaskKey,
        _success: bool,
        _fallback_result: Option<&'static str>,
    ) {
        if let Some(buffer) = self.control.tool_buffers.get_mut(&key) {
            buffer.pending_tool_cards.clear();
        }
    }

    /// `Action::ToolStarted` — 追加 Running 状态的 ToolResult 卡片 + 记录索引.
    #[cfg(feature = "terminal-tui")]
    fn reduce_tool_started(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        _sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        args: String,
    ) -> Vec<Effect> {
        use crate::chat::tui::{
            ARGS_PREVIEW_ELLIPSIS, ARGS_PREVIEW_MAX_CHARS, ConversationLine, ToolStatus, build_tool_args_preview,
        };
        let args_preview = build_tool_args_preview(&name, &args, ARGS_PREVIEW_MAX_CHARS, ARGS_PREVIEW_ELLIPSIS);
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        self.control
            .tool_buffer_mut(tool_key)
            .tool_args
            .insert(invocation_key, args_preview.clone());
        self.ui.conversation_lines.push(ConversationLine::ToolResult {
            tool_name: name,
            args_preview,
            args_full: args,
            result: None,
            status: ToolStatus::Running,
            elapsed_ms: None,
            folded: true,
        });
        let idx = self.ui.conversation_lines.len().saturating_sub(1);
        self.control.tool_buffer_mut(tool_key).pending_tool_cards.push(idx);
        self.refresh_provider_worker_view_if_focused();
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_tool_started(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        _sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        args: String,
    ) -> Vec<Effect> {
        let args_preview = if args.chars().count() > 80 {
            let prefix: String = args.chars().take(80).collect();
            format!("{prefix}…")
        } else {
            args.clone()
        };
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        self.control
            .tool_buffer_mut(tool_key)
            .tool_args
            .insert(invocation_key, args_preview);
        self.ui.conversation_lines.push(format!("tool_started:{name}:{args}"));
        let idx = self.ui.conversation_lines.len().saturating_sub(1);
        self.control.tool_buffer_mut(tool_key).pending_tool_cards.push(idx);
        vec![Effect::RequestRedraw]
    }

    /// `Action::ToolFinished` — 更新对应 Running 卡片 → Done/Error.
    #[cfg(feature = "terminal-tui")]
    fn reduce_tool_finished(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        success: bool,
        duration_ms: u64,
        result: Option<String>,
    ) -> Vec<Effect> {
        use crate::chat::session::ToolCallSummary;
        use crate::chat::tui::{ConversationLine, ToolStatus};
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let sequence = sequence.or_else(|| self.sequence_for_tool_key(tool_key));
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        let buffer = self.control.tool_buffer_mut(tool_key);
        let args_preview = buffer.tool_args.remove(&invocation_key).unwrap_or_default();
        buffer.tool_calls.push(ToolCallSummary {
            name: name.clone(),
            args_preview,
            success,
            task_id: task_id.map(crate::chat::turn_scheduler::TurnTaskId::get),
            sequence,
        });
        // 第 1 步：从 pending_tool_cards 反向查找最近一个 name 匹配 + Running 的卡片
        // （只借用 conversation_lines，不持 mut 引用，避免 result 跨循环 move 冲突）
        let target_pos = self.control.tool_buffers.get(&tool_key).and_then(|buffer| {
            buffer
                .pending_tool_cards
                .iter()
                .enumerate()
                .rev()
                .find_map(|(pos, &idx)| match self.ui.conversation_lines.get(idx) {
                    Some(ConversationLine::ToolResult { tool_name, status, .. })
                        if tool_name == &name && *status == ToolStatus::Running =>
                    {
                        Some((pos, idx))
                    }
                    _ => None,
                })
        });
        // 第 2 步：找到目标后再做 mut 更新 + 从 pending 移除
        if let Some((pending_pos, line_idx)) = target_pos {
            if let Some(ConversationLine::ToolResult {
                status,
                elapsed_ms,
                result: result_slot,
                ..
            }) = self.ui.conversation_lines.get_mut(line_idx)
            {
                *status = if success { ToolStatus::Done } else { ToolStatus::Error };
                *elapsed_ms = Some(duration_ms);
                *result_slot = result;
            }
            if let Some(buffer) = self.control.tool_buffers.get_mut(&tool_key) {
                buffer.pending_tool_cards.remove(pending_pos);
            }
        }
        self.refresh_provider_worker_view_if_focused();
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_finished name={name} success={success} duration_ms={duration_ms}"),
            },
        ]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_tool_finished(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        sequence: Option<u64>,
        tool_call_id: Option<String>,
        name: String,
        success: bool,
        duration_ms: u64,
        _result: Option<String>,
    ) -> Vec<Effect> {
        use crate::chat::session::ToolCallSummary;
        let tool_key = ToolTaskKey::from_task_id(task_id);
        let sequence = sequence.or_else(|| self.sequence_for_tool_key(tool_key));
        let invocation_key = ToolInvocationKey::new(tool_call_id, &name);
        let buffer = self.control.tool_buffer_mut(tool_key);
        let args_preview = buffer.tool_args.remove(&invocation_key).unwrap_or_default();
        buffer.tool_calls.push(ToolCallSummary {
            name: name.clone(),
            args_preview,
            success,
            task_id: task_id.map(crate::chat::turn_scheduler::TurnTaskId::get),
            sequence,
        });
        // 占位 feature 下仅记录 + 弹出最后一个 pending 索引
        if let Some(buffer) = self.control.tool_buffers.get_mut(&tool_key)
            && !buffer.pending_tool_cards.is_empty()
        {
            buffer.pending_tool_cards.pop();
        }
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_finished name={name} success={success} duration_ms={duration_ms}"),
            },
        ]
    }

    /// `Action::ToolProgress` — 进度通知（仅 RequestRedraw + LogTrace）.
    ///
    /// 当前 UI 未单独显示 progress 字段；保留 Action 是为了未来扩展 + 钩子触发.
    /// 不 mutate UI 状态（签名仍接受 `&self` 但 reducer 入口统一传 `&mut`，
    /// 此处用 `&self` 让 clippy::needless-pass-by-ref-mut 静音）.
    fn reduce_tool_progress(&mut self, iteration: usize, max: usize) -> Vec<Effect> {
        self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_progress {iteration}/{max}"),
            },
            Effect::RequestRedraw,
        ]
    }

    /// **S3 T3-1**: `Action::ToolApprovalRequested` — records the foreground
    /// approval view and asks the EffectExecutor to surface it.
    ///
    /// driver 在 supervised autonomy 模式下，**先于** ToolStarted 发送该 Action，
    /// 让 reducer 把请求转给 EffectExecutor / UI；driver 自己通过 oneshot rx
    /// 等响应（dispatcher 把 `ToolApprovalReceived` 转写到 driver 的接收 channel）。
    /// reducer only owns display state. The driver/router remains the single
    /// approval owner and execution gate.
    fn reduce_tool_approval_requested(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        tool_id: String,
        name: String,
        args: String,
    ) -> Vec<Effect> {
        self.ui.pending_tool_approval = Some(crate::chat::sessions::PendingToolApprovalView {
            task_id,
            tool_id: tool_id.clone(),
            name: name.clone(),
            args: args.clone(),
        });
        self.ui.focus = crate::chat::sessions::FocusTarget::Approval;
        self.ui.switcher = None;
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_approval_requested tool_id={tool_id} name={name}"),
            },
            Effect::RequestApproval {
                task_id,
                tool_id,
                name,
                args,
            },
        ]
    }

    /// **S3 T3-1**: `Action::ToolApprovalReceived` — clear the display prompt
    /// after a human/non-TUI decision. The dispatcher resolves the router after
    /// this reducer step.
    fn reduce_tool_approval_received(&mut self, tool_id: &str, approved: bool) -> Vec<Effect> {
        if self
            .ui
            .pending_tool_approval
            .as_ref()
            .is_some_and(|view| view.tool_id == tool_id)
        {
            self.ui.pending_tool_approval = None;
            if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
                self.ui.focus = crate::chat::sessions::FocusTarget::Main;
            }
        }
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("tool_approval_received tool_id={tool_id} approved={approved}"),
            },
            Effect::RequestRedraw,
        ]
    }

    fn reduce_tool_approval_cleared(&mut self) -> Vec<Effect> {
        self.ui.pending_tool_approval = None;
        if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
            self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        }
        vec![
            Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: "tool_approval_cleared".to_string(),
            },
            Effect::RequestRedraw,
        ]
    }

    /// **S3 T3-1**: `Action::StreamRetryAttempt` — 网络重试尝试，仅 trace + 重绘.
    ///
    /// 不 mutate state（driver 自己维护 attempt 计数）；UI 可据此显示 "retrying..." 提示。
    fn reduce_stream_retry_attempt(&self, attempt: u8, reason: &str) -> Vec<Effect> {
        let _ = &self.ui;
        vec![
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("stream_retry_attempt #{attempt} reason={reason}"),
            },
            Effect::RequestRedraw,
        ]
    }

    // ── Step 4 子函数（退出 + 会话） ─────────────────────────────────────────────

    /// `Action::CancelRequested` — 单击 Ctrl+C，取消当前流式回合（如有）.
    ///
    /// - 若 generating == false → 无活动回合，返回 vec![]（no-op）
    /// - 若 generating == true  → 清除 stream.draft + control 状态，返回
    ///   [CancelToken(tok)?, CancelDraft(id), LogTrace, RequestRedraw]
    ///
    /// S2-B Step 2: 新增 [`Effect::CancelToken`] — 在 reducer 把 `active_cancel.take()`
    /// 取出后立刻发给 EffectExecutor，由它真调 `token.cancel()`。这关闭了之前
    /// "UI 已 cancel 但底层 LLM 流仍在跑" 的窗口（reducer 仅清状态、不 cancel token
    /// 是 S2-B Codex 风险点）。
    fn reduce_cancel_requested(&mut self) -> Vec<Effect> {
        if !self.control.generating {
            // 空闲时取消无意义 — no-op
            return vec![];
        }
        let Some((tool_key, draft_id)) = self.primary_cancel_target() else {
            return vec![];
        };
        self.cancel_task(tool_key, draft_id, "turn cancelled by cancel request")
    }

    fn primary_cancel_target(&self) -> Option<(ToolTaskKey, String)> {
        self.stream
            .primary_draft()
            .map(|turn| (ToolTaskKey::from_task_id(turn.task_id), turn.draft.draft_id.clone()))
    }

    fn reduce_cancel_provider_turn(&mut self, task_id: crate::chat::turn_scheduler::TurnTaskId) -> Vec<Effect> {
        let Some(draft_id) = self
            .stream
            .visible_drafts
            .iter()
            .find(|turn| turn.task_id == Some(task_id))
            .map(|turn| turn.draft.draft_id.clone())
        else {
            return vec![];
        };
        self.cancel_task(
            ToolTaskKey::Task(task_id),
            draft_id,
            "turn cancelled by provider worker cancel request",
        )
    }

    fn clear_target_pending_approval(
        &mut self,
        tool_key: ToolTaskKey,
    ) -> Option<crate::chat::sessions::PendingToolApprovalView> {
        let should_clear = self
            .ui
            .pending_tool_approval
            .as_ref()
            .is_some_and(|pending| ToolTaskKey::from_task_id(pending.task_id) == tool_key);
        if !should_clear {
            return None;
        }
        let pending = self.ui.pending_tool_approval.take();
        if matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
            self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        }
        pending
    }

    fn cancel_task(&mut self, tool_key: ToolTaskKey, draft_id: String, reason: &'static str) -> Vec<Effect> {
        let cancel_opt = self.control.take_turn_cancel(tool_key);
        let _ = self.stream.remove_visible_draft(&draft_id);
        self.finalize_pending_tool_cards(tool_key, false, Some(reason));
        self.control.clear_tool_buffer(tool_key);
        let target_pending_approval = self.clear_target_pending_approval(tool_key);
        let no_visible_drafts = !self.stream.has_visible_drafts();
        let no_task_cancels = !self.control.has_task_turn_cancels();
        let global_pending_approval = if no_visible_drafts && no_task_cancels {
            self.control.active_cancel = None;
            self.control.generating = false;
            let pending = self.ui.pending_tool_approval.take();
            if pending.is_some() && matches!(self.ui.focus, crate::chat::sessions::FocusTarget::Approval) {
                self.ui.focus = crate::chat::sessions::FocusTarget::Main;
            }
            pending
        } else {
            None
        };

        let mut effects = Vec::new();
        // 优先发 CancelToken 真触发底层取消；再发 CancelDraft 同步 channel UI。
        if let Some(token) = cancel_opt {
            effects.push(Effect::CancelToken(token));
        }
        for pending in target_pending_approval.into_iter().chain(global_pending_approval) {
            effects.push(Effect::ResolveApproval {
                tool_id: pending.tool_id,
                approved: false,
            });
        }
        effects.push(Effect::CancelDraft(draft_id));
        effects.push(Effect::LogTrace {
            level: tracing::Level::INFO,
            msg: "Turn cancelled by CancelRequested".to_string(),
        });
        effects.push(Effect::RequestRedraw);
        effects
    }

    /// `Action::ShutdownRequested` — 双击 Ctrl+C / SIGTERM，优雅退出.
    ///
    /// 若正在生成则一并取消当前 draft + token，然后返回 [Quit]。
    /// 主循环看到 `Effect::Quit` 后调用 `shutdown.cancel()`（CancellationToken 持有在
    /// 主循环外壳，reducer 不直接持有，Step 5 完整接线时确认）。
    ///
    /// S2-B Step 2: 与 [`Self::reduce_cancel_requested`] 一致 — 流式 turn 还活着时
    /// 必须发 `Effect::CancelToken` 让 EffectExecutor 真调 token.cancel()，否则
    /// 底层 LLM 流不会立刻收到 cancel 信号。
    fn reduce_shutdown_requested(&mut self) -> Vec<Effect> {
        let (draft_id_opt, cancel_tokens) = if self.control.generating {
            let id = Self::take_draft_id(&self.stream);
            let tokens = self.control.drain_turn_cancels();
            self.stream.clear_visible_drafts();
            self.control.generating = false;
            (id, tokens)
        } else {
            (None, Vec::new())
        };

        let mut effects = Vec::new();
        for token in cancel_tokens {
            effects.push(Effect::CancelToken(token));
        }
        if let Some(draft_id) = draft_id_opt {
            effects.push(Effect::CancelDraft(draft_id));
        }
        effects.push(Effect::Quit);
        effects
    }

    /// `Action::SessionLoaded(ChatSession)` — 恢复持久化会话到 SessionState.
    ///
    /// 全字段替换（id/title/provider/model/mode/turns）；history 由主循环在
    /// SessionLoaded 到来时从 turns 重建（Step 5 接线）。
    fn reduce_session_loaded(&mut self, loaded: ChatSession) -> Vec<Effect> {
        let id = loaded.id.clone();
        if self.control.generating {
            return vec![Effect::LogTrace {
                level: tracing::Level::WARN,
                msg: format!("SessionLoaded rejected while generating: {id}"),
            }];
        }
        self.session.id = loaded.id;
        self.session.title = loaded.title;
        self.session.provider = Arc::from(loaded.provider.as_str());
        self.session.model = Arc::from(loaded.model.as_str());
        self.session.mode = loaded.mode;
        self.ui.chat_mode = loaded.mode;
        self.session.turns = loaded.turns;
        self.session.token_usage_records = loaded.token_usage_records;
        self.ui.token_usage_summary = MainSessionTokenUsageSummary::from_records(&self.session.token_usage_records);
        // v4: restore persisted background-session summaries (display only —
        // the live processes are gone and are never revived). Carrying them in
        // SessionState means the next save_session snapshot re-persists them, so
        // they survive across multiple reload cycles.
        self.session.background_sessions = loaded.background_sessions;
        // S4-B T4-B-6: 保留原 session 的 created_at，避免下次 save_session 覆盖
        self.session.created_at = Some(loaded.created_at);
        // history 从 turns 重建（仅 user/assistant 角色进 LLM context）
        self.session.history = self
            .session
            .turns
            .iter()
            .filter(|t| t.role == "user" || t.role == "assistant")
            .map(|t| ChatMessage {
                role: t.role.clone(),
                content: t.content.clone(),
            })
            .collect();
        self.ui.conversation_lines = conversation_lines_from_turns(&self.session.turns);
        self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        #[cfg(feature = "terminal-tui")]
        self.ui.input.clear_navigation_state();
        #[cfg(not(feature = "terminal-tui"))]
        self.ui.input.clear();
        self.ui.turn_count = self.session.turns.len();
        self.ui.active_session_view = None;
        self.ui.pending_tool_approval = None;
        self.ui.context_used_tokens = None;
        self.ui.context_window_tokens = None;
        self.ui.focus = crate::chat::sessions::FocusTarget::Main;
        self.ui.sessions_status.clear();
        self.ui.sessions_entries.clear();
        self.ui.switcher = None;
        self.ui.slash_menu = None;
        self.ui.saved_session_picker = None;
        self.stream.clear_visible_drafts();
        self.control.clear_all_tool_buffers();
        self.control.turn_cancels.clear();
        self.control.final_usage_tasks_recorded.clear();
        self.control.generating = false;
        self.control.active_cancel = None;
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("Session loaded: {id}"),
            },
        ]
    }

    /// `Action::SessionSaved { id }` — 更新会话 id（首次保存时服务端可能分配新 id）.
    fn reduce_session_saved(&mut self, id: String) -> Vec<Effect> {
        if self.session.id != id {
            self.session.id = id.clone();
        }
        vec![Effect::LogTrace {
            level: tracing::Level::INFO,
            msg: format!("Session saved: {id}"),
        }]
    }

    /// `Action::SessionSwitched { id }` — 请求切换到另一个会话.
    ///
    /// 设计：两步异步流程，reducer 只负责 effects[0] = SaveSession(current)。
    /// 主循环执行 save 后，spawn 异步加载并 dispatch `SessionLoaded(new_session)`。
    /// 中断窗口（save 成功前崩溃）由主循环的 try/catch 处理，不在 reducer 内。
    ///
    /// effects 顺序（精确）：
    ///   [0] SaveSession(current_snapshot)
    ///   [1] LogTrace
    ///   [2] RequestRedraw
    fn reduce_session_switched(&self, id: String) -> Vec<Effect> {
        vec![
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: format!("Session switching to: {id}"),
            },
            Effect::RequestRedraw,
        ]
    }

    /// T3-3-c: 从 `SessionState` 构造一个 [`ChatSession`] 快照，用于 [`Effect::SaveSession`].
    ///
    /// `SessionState` 不持有 `created_at` / `updated_at` 时间戳（chronological 元数据由
    /// `ChatSession` 持久化层管理），因此快照构造时用当前时间填充——既能区分多次保存的
    /// `updated_at`，也允许 `load_latest_session` 用 `updated_at` 比较选最新会话。
    /// `schema_version` 用 `SCHEMA_VERSION` 常量统一。
    ///
    /// 抽出独立 fn 让 `reduce_session_switched` / `reduce_stream_completed` 等多处共用同一
    /// 构造路径，避免字段错漏。
    fn build_session_snapshot(&self) -> ChatSession {
        let now = chrono::Utc::now();
        ChatSession {
            id: self.session.id.clone(),
            schema_version: crate::chat::session::SCHEMA_VERSION,
            title: self.session.title.clone(),
            provider: self.session.provider.as_ref().to_owned(),
            model: self.session.model.as_ref().to_owned(),
            // S4-B T4-B-6: created_at 严格语义 — 取 SessionState.created_at（首次 RecordUserTurn 初始化），
            // 兜底用 now，保证不会反向覆盖既有创建时间
            created_at: self.session.created_at.unwrap_or(now),
            updated_at: now,
            turns: self.session.turns.clone(),
            background_sessions: self.session.background_sessions.clone(),
            token_usage_records: self.session.token_usage_records.clone(),
            mode: self.session.mode,
        }
    }

    /// `Action::RecordUserTurn(text)` — 请求 reducer 持久化用户回合到 session 记录和 LLM history.
    ///
    /// 对齐 `session.add_user_turn` 语义：
    /// - `updated_at` 由 effect executor 在构建 `SaveSession` 快照时设置（SessionState 不含时间戳）
    /// - 首条 user turn 时若 title 为空则自动 set_title（截断前 50 字符，对齐 ChatSession 逻辑）
    /// - tool_calls 留空，tool 同步由 `ToolStarted`/`ToolFinished` 单独处理（Step 5b）
    fn reduce_record_user_turn(&mut self, content: String) -> Vec<Effect> {
        let now = chrono::Utc::now();
        // S4-B T4-B-6: 首次 RecordUserTurn 延迟初始化 created_at
        if self.session.created_at.is_none() {
            self.session.created_at = Some(now);
        }
        self.session.turns.push(crate::chat::session::ChatTurn {
            role: "user".to_string(),
            content: content.clone(),
            timestamp: now,
            tool_calls: Vec::new(),
        });
        // 首条 user turn 且 title 为空时自动设置标题（对齐 session.add_user_turn 行为）
        if self.session.title.is_empty() {
            self.session.title = crate::chat::session::truncate_title(&content);
        }
        self.session.history.push(ChatMessage::user(content));
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("RecordUserTurn len={}", self.session.turns.len()),
        }]
    }

    /// `Action::RecordAssistantTurn` — 请求 reducer 持久化助手回合到 session 记录和 LLM history.
    ///
    /// 对齐 `session.add_assistant_turn` 语义：
    /// - `updated_at` 由 effect executor 在构建 `SaveSession` 快照时设置（SessionState 不含时间戳）
    /// - P3a: tool_calls come from the matching task bucket. Legacy callers use
    ///   the Primary bucket, so main transcript behavior stays unchanged.
    fn reduce_record_assistant_turn(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        content: String,
    ) -> Vec<Effect> {
        let tool_calls = self.control.take_tool_calls(ToolTaskKey::from_task_id(task_id));
        self.session.turns.push(crate::chat::session::ChatTurn {
            role: "assistant".to_string(),
            content: content.clone(),
            timestamp: chrono::Utc::now(),
            tool_calls,
        });
        self.session.history.push(ChatMessage::assistant(content));
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("RecordAssistantTurn len={}", self.session.turns.len()),
        }]
    }

    /// `Action::RecordSystemMessage` — append 一条 system 消息到 LLM context history.
    ///
    /// S2-C Step 2: 与 legacy `history.push(ChatMessage::system(content))` 对齐.
    /// 与 [`Self::reduce_set_leading_system_prompt`] 的区别:
    /// - 本函数永远 append（典型场景: `/clear` 后重建 system prompt — clear 已经把
    ///   history 清空，新 system 直接 push 到末尾即首位，append 与替换等价）
    /// - `SetLeadingSystemPrompt` 做 upsert（empty → push，非空 → 替换 history[0]）
    ///
    /// session.turns 不更新（system 消息不是用户/助手"回合"，仅是 LLM 上下文配置）。
    fn reduce_record_system_message(&mut self, content: String) -> Vec<Effect> {
        self.session.history.push(ChatMessage::system(content));
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("RecordSystemMessage history_len={}", self.session.history.len()),
        }]
    }

    /// `Action::SetLeadingSystemPrompt` — set/replace 首位 system prompt.
    ///
    /// S2-C Step 2: 与 chat::mod 主循环 `if history.is_empty() { push } else {
    /// first_mut = system }` 字节级对齐。每轮 turn 都会跑（technique selection
    /// 后重建 system prompt），用 append 表达会让 history 越长越多 system 消息。
    ///
    /// 行为:
    /// - history 为空 → push 一条 system
    /// - history 非空且首位为 system → 替换 `history[0]` 内容（与 legacy `*first = ...` 一致）
    /// - history 非空但首位**不**是 system → insert system at the front. This keeps
    ///   resumed user/assistant turns intact when a loaded session rebuilds history
    ///   without a runtime system prompt.
    fn reduce_set_leading_system_prompt(&mut self, content: String) -> Vec<Effect> {
        if self.session.history.is_empty() {
            self.session.history.push(ChatMessage::system(content));
        } else if self
            .session
            .history
            .first()
            .is_some_and(|message| message.role != "system")
        {
            self.session.history.insert(0, ChatMessage::system(content));
        } else if let Some(first) = self.session.history.first_mut() {
            *first = ChatMessage::system(content);
        }
        vec![Effect::LogTrace {
            level: tracing::Level::DEBUG,
            msg: format!("SetLeadingSystemPrompt history_len={}", self.session.history.len()),
        }]
    }

    /// `Action::SystemMessageAdded` — append 一条 system 消息到 Redux UI 镜像.
    ///
    /// S2-C Step 2: 与 legacy `chat_mirror.lock().push_system_message(text)` 双写.
    /// reducer 在 `ui.conversation_lines` 维护一份 ConversationLine::System，
    /// 让 Redux 路径有自己的 UI 账本；真实可见 TUI 仍由 `chat_mirror` 渲染，
    /// 本 reducer 不替代 mirror（chat_mirror 的写仍在 mod.rs unconditional 跑）。
    ///
    /// 非 terminal-tui feature 下仅发 RequestRedraw（占位类型 String 不语义化），
    /// 与其他 TUI-only push 函数（user/assistant）行为对称。
    #[cfg(feature = "terminal-tui")]
    fn reduce_system_message_added(&mut self, text: String) -> Vec<Effect> {
        self.ui
            .conversation_lines
            .push(crate::chat::tui::ConversationLine::System { content: text });
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_system_message_added(&mut self, _text: String) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// `Action::UserMessageEchoed` — Pure 模式下用户提交的视觉 echo
    #[cfg(feature = "terminal-tui")]
    fn reduce_user_message_echoed(&mut self, text: String) -> Vec<Effect> {
        self.ui
            .conversation_lines
            .push(crate::chat::tui::ConversationLine::User { content: text });
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn reduce_user_message_echoed(&mut self, _text: String) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// `Action::SessionsStatusUpdated` — replace the persistent background-session
    /// status line (v1b). The main loop already dedups (only dispatches when the
    /// summary changed), but we still no-op an identical write so a stray
    /// duplicate cannot mark the UI dirty for nothing.
    fn reduce_sessions_status_updated(&mut self, summary: String) -> Vec<Effect> {
        if self.ui.sessions_status == summary {
            return Vec::new();
        }
        self.ui.sessions_status = summary;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SessionsEntriesUpdated` — replace the structured child-session
    /// entries that back the P1 bottom strip. Identical snapshots are no-ops so
    /// the 1s session poll cannot churn redraws when nothing changed.
    fn reduce_sessions_entries_updated(&mut self, entries: Vec<crate::chat::sessions::SwitcherEntry>) -> Vec<Effect> {
        if self.ui.sessions_entries == entries {
            return Vec::new();
        }
        self.ui.sessions_entries = entries;
        vec![Effect::RequestRedraw]
    }

    fn reduce_main_queue_status_updated(&mut self, status: MainQueueStatus) -> Vec<Effect> {
        if self.ui.main_queue_status == status {
            return Vec::new();
        }
        self.ui.main_queue_status = status;
        vec![Effect::RequestRedraw]
    }

    fn reduce_provider_worker_status_updated(&mut self, status: ProviderWorkerStatus) -> Vec<Effect> {
        if self.ui.provider_worker_status == status {
            return Vec::new();
        }
        let worker_view = self.ui.focus.worker_sequence().map(|sequence| {
            let previous_view = self
                .ui
                .active_session_view
                .as_ref()
                .filter(|view| view.kind == crate::chat::action::PROVIDER_WORKER_VIEW_KIND && view.seq == sequence);
            #[cfg(feature = "terminal-tui")]
            let io_lines = crate::chat::tui::provider_worker_io_lines_for_streaming_draft(
                &self.ui.conversation_lines,
                self.stream.streaming_draft_for_worker(sequence),
                12,
            );
            #[cfg(not(feature = "terminal-tui"))]
            let io_lines = Vec::new();
            crate::chat::action::build_provider_worker_active_view_with_io_preserving_scroll(
                &status,
                sequence,
                previous_view,
                io_lines,
            )
        });
        self.ui.provider_worker_status = status;
        if let Some(view) = worker_view {
            self.ui.active_session_view = Some(view);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(feature = "terminal-tui")]
    fn refresh_provider_worker_view_if_focused(&mut self) {
        let Some(sequence) = self.ui.focus.worker_sequence() else {
            return;
        };
        let previous_view = self
            .ui
            .active_session_view
            .as_ref()
            .filter(|view| view.kind == crate::chat::action::PROVIDER_WORKER_VIEW_KIND && view.seq == sequence);
        let io_lines = crate::chat::tui::provider_worker_io_lines_for_streaming_draft(
            &self.ui.conversation_lines,
            self.stream.streaming_draft_for_worker(sequence),
            12,
        );
        self.ui.active_session_view = Some(
            crate::chat::action::build_provider_worker_active_view_with_io_preserving_scroll(
                &self.ui.provider_worker_status,
                sequence,
                previous_view,
                io_lines,
            ),
        );
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn refresh_provider_worker_view_if_focused(&mut self) {}

    #[cfg(feature = "terminal-tui")]
    fn reduce_slash_menu_sources_updated(
        &mut self,
        saved_sessions: Vec<crate::chat::session::SavedSessionPickerEntry>,
        provider_model_catalog: Vec<crate::chat::slash_types::SlashProviderModelCatalog>,
    ) -> Vec<Effect> {
        if self.ui.saved_sessions_cache == saved_sessions && self.ui.provider_model_catalog == provider_model_catalog {
            return Vec::new();
        }
        self.ui.saved_sessions_cache = saved_sessions;
        self.ui.provider_model_catalog = provider_model_catalog;
        if self.ui.slash_menu.is_some() {
            let sources = Self::slash_menu_sources_from(
                &self.ui.sessions_entries,
                &self.ui.saved_sessions_cache,
                &self.ui.provider_model_catalog,
                &self.ui.at_path_candidates,
                self.session.provider.as_ref(),
            );
            crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        }
        vec![Effect::RequestRedraw]
    }

    #[cfg(feature = "terminal-tui")]
    fn reduce_at_path_candidates_updated(&mut self, candidates: Vec<AtPathCandidate>) -> Vec<Effect> {
        if self.ui.at_path_candidates == candidates {
            return Vec::new();
        }
        self.ui.at_path_candidates = candidates;
        let sources = Self::slash_menu_sources_from(
            &self.ui.sessions_entries,
            &self.ui.saved_sessions_cache,
            &self.ui.provider_model_catalog,
            &self.ui.at_path_candidates,
            self.session.provider.as_ref(),
        );
        crate::chat::tui::sync_slash_menu_for_sources(&self.ui.input, &mut self.ui.slash_menu, sources);
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_at_path_candidates_updated(&mut self, _candidates: Vec<AtPathCandidate>) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn reduce_slash_menu_sources_updated(
        &mut self,
        _saved_sessions: Vec<crate::chat::session::SavedSessionPickerEntry>,
        _provider_model_catalog: Vec<crate::chat::slash_types::SlashProviderModelCatalog>,
    ) -> Vec<Effect> {
        vec![Effect::RequestRedraw]
    }

    /// `Action::ActiveSessionViewUpdated` — replace/clear the focused child
    /// viewport render snapshot.
    fn reduce_active_session_view_updated(
        &mut self,
        view: Option<crate::chat::sessions::ActiveSessionView>,
    ) -> Vec<Effect> {
        if self.ui.active_session_view == view {
            return Vec::new();
        }
        self.ui.active_session_view = view;
        vec![Effect::RequestRedraw]
    }

    fn reduce_context_window_updated(
        &mut self,
        used_context_tokens: Option<usize>,
        max_context_tokens: Option<usize>,
    ) -> Vec<Effect> {
        if self.ui.context_used_tokens == used_context_tokens && self.ui.context_window_tokens == max_context_tokens {
            return Vec::new();
        }
        self.ui.context_used_tokens = used_context_tokens;
        self.ui.context_window_tokens = max_context_tokens;
        vec![Effect::RequestRedraw]
    }

    fn reduce_provider_usage_recorded(
        &mut self,
        task_id: Option<crate::chat::turn_scheduler::TurnTaskId>,
        usage_kind: ProviderUsageRecordKind,
        record: MainSessionTokenUsageRecord,
    ) -> Vec<Effect> {
        if !self.control.should_record_provider_usage(task_id, usage_kind) {
            return Vec::new();
        }
        self.session.token_usage_records.push(record);
        self.ui.token_usage_summary = MainSessionTokenUsageSummary::from_records(&self.session.token_usage_records);
        vec![
            Effect::SaveSession(self.build_session_snapshot()),
            Effect::RequestRedraw,
        ]
    }

    /// `Action::BackgroundSessionRecorded` (v4) — upsert a background-session
    /// summary into `session.background_sessions` and **immediately emit
    /// `Effect::SaveSession`** so the summary is durably persisted to the memory
    /// backend (the only write path; `dispatcher.rs` `Effect::SaveSession`).
    ///
    /// Why emit SaveSession here (P0, v4 review): under `terminal-tui` (now the
    /// default) the legacy exit-save path is disabled (`mod.rs`
    /// `legacy_exit_save_enabled=false`), and no other action snapshots after a
    /// child session reaches a terminal state. Without this effect the
    /// summary lived only in memory and was lost on exit → reload recap broke.
    /// Emitting SaveSession **after** the upsert guarantees the snapshot
    /// (`build_session_snapshot`, which clones `self.session.background_sessions`)
    /// already contains this record, eliminating the prior race where a snapshot
    /// taken before the action could miss it.
    ///
    /// Dedup is by session id: a later record for the same id replaces the
    /// earlier one (e.g. an `interrupted` entry written at exit, or a terminal
    /// summary superseding a placeholder). This records **summary only** — it
    /// never spawns or revives a process / sub-agent / PTY.
    ///
    /// No save storm: a child session reaching a terminal state is a
    /// low-frequency event, and an unchanged re-record short-circuits to
    /// `Vec::new()` before emitting any effect. `Effect::SaveSession` is a pure
    /// persistence sink (it never dispatches a new action), so there is no
    /// SaveSession → BackgroundSessionRecorded feedback loop.
    fn reduce_background_session_recorded(
        &mut self,
        summary: crate::chat::sessions::PersistedSessionSummary,
    ) -> Vec<Effect> {
        if let Some(existing) = self.session.background_sessions.iter_mut().find(|s| s.id == summary.id) {
            if *existing == summary {
                return Vec::new();
            }
            *existing = summary;
        } else {
            self.session.background_sessions.push(summary);
        }
        // Persist the updated snapshot now (state already mutated above).
        vec![Effect::SaveSession(self.build_session_snapshot())]
    }

    /// `Action::SessionFocusChanged` (v1.1b) — record the current input-routing
    /// target so the snapshot prompt indicator (colour+glyph) reflects it.
    /// Idempotent: an unchanged focus is a no-op (no needless redraw).
    fn reduce_session_focus_changed(&mut self, focus: crate::chat::sessions::FocusTarget) -> Vec<Effect> {
        if self.ui.focus == focus {
            return Vec::new();
        }
        self.ui.focus = focus;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SwitcherOpened` (v1.1b) — open the Ctrl+G switcher overlay over
    /// the supplied session snapshot, highlighting the first row.
    fn reduce_switcher_opened(&mut self, entries: Vec<crate::chat::sessions::SwitcherEntry>) -> Vec<Effect> {
        self.ui.saved_session_picker = None;
        self.ui.slash_menu = None;
        self.ui.switcher = Some(crate::chat::sessions::SwitcherState::new(entries));
        vec![Effect::RequestRedraw]
    }

    /// `Action::SwitcherMoved` (v1.1b) — update the highlighted row. The index is
    /// clamped to a valid row by the key thread; we clamp again defensively so a
    /// stale snapshot can never index out of range. No-op (no redraw) when the
    /// switcher is closed or the selection is unchanged.
    fn reduce_switcher_moved(&mut self, selected: usize) -> Vec<Effect> {
        let Some(switcher) = self.ui.switcher.as_mut() else {
            return Vec::new();
        };
        let clamped = if switcher.entries.is_empty() {
            0
        } else {
            selected.min(switcher.entries.len().saturating_sub(1))
        };
        if switcher.selected == clamped {
            return Vec::new();
        }
        switcher.selected = clamped;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SwitcherClosed` (v1.1b) — close the switcher overlay. No-op (no
    /// redraw) when already closed.
    fn reduce_switcher_closed(&mut self) -> Vec<Effect> {
        if self.ui.switcher.is_none() {
            return Vec::new();
        }
        self.ui.switcher = None;
        vec![Effect::RequestRedraw]
    }

    /// `Action::StripSelectionChanged` — update the UI-only bottom-strip
    /// highlight without changing input-routing focus.
    fn reduce_strip_selection_changed(&mut self, selected: Option<u64>) -> Vec<Effect> {
        if self.ui.strip_selection == selected {
            return Vec::new();
        }
        self.ui.strip_selection = selected;
        vec![Effect::RequestRedraw]
    }

    /// `Action::SavedSessionPickerOpened` (P7c) — open the saved chat-session
    /// history picker, separate from the child-TUI Ctrl+G switcher.
    fn reduce_saved_session_picker_opened(
        &mut self,
        entries: Vec<crate::chat::session::SavedSessionPickerEntry>,
    ) -> Vec<Effect> {
        self.ui.switcher = None;
        self.ui.slash_menu = None;
        self.ui.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(entries));
        vec![Effect::RequestRedraw]
    }

    fn reduce_saved_session_picker_moved(&mut self, selected: usize) -> Vec<Effect> {
        let Some(picker) = self.ui.saved_session_picker.as_mut() else {
            return Vec::new();
        };
        let clamped = if picker.entries.is_empty() {
            0
        } else {
            selected.min(picker.entries.len().saturating_sub(1))
        };
        if picker.selected == clamped {
            return Vec::new();
        }
        picker.selected = clamped;
        vec![Effect::RequestRedraw]
    }

    fn reduce_saved_session_picker_closed(&mut self) -> Vec<Effect> {
        if self.ui.saved_session_picker.is_none() {
            return Vec::new();
        }
        self.ui.saved_session_picker = None;
        vec![Effect::RequestRedraw]
    }

    #[cfg(feature = "terminal-tui")]
    fn reduce_saved_session_picker_key_pressed(&mut self, key: crossterm::event::KeyEvent) -> Vec<Effect> {
        use crossterm::event::{KeyCode, KeyModifiers};

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let up = key.code == KeyCode::Up || (ctrl && key.code == KeyCode::Char('p'));
        let down = key.code == KeyCode::Down || (ctrl && key.code == KeyCode::Char('n'));
        if up || down {
            let Some(picker) = self.ui.saved_session_picker.as_mut() else {
                return Vec::new();
            };
            if up {
                picker.select_prev();
            } else {
                picker.select_next();
            }
            picker.clamp_selected();
            return vec![Effect::RequestRedraw];
        }
        if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
            return self.reduce_saved_session_picker_closed();
        }
        if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
            return self.reduce_saved_session_picker_closed();
        }
        Vec::new()
    }

    /// `Action::HistoryCleared` — 清除 LLM context history（保留 system prompt）+ 清 UI.
    ///
    /// session.turns 不清除（持久化记录不可逆）；只重置 LLM context（下次请求
    /// 不带历史消息）和 TUI conversation_lines 显示。
    fn reduce_history_cleared(&mut self) -> Vec<Effect> {
        // 防御性保留所有 system 消息（通常只有 1 条，但扫描全部以防 system 不在首位）
        let system_msgs: Vec<_> = self.session.history.drain(..).filter(|m| m.role == "system").collect();
        // history 已由 drain(..) 清空，重新插入 system 消息
        self.session.history.extend(system_msgs);
        // 注: 当前 input buffer 不清理，由 InputCancelled 单独处理
        self.ui.conversation_lines.clear();
        self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
        vec![
            Effect::RequestRedraw,
            Effect::LogTrace {
                level: tracing::Level::INFO,
                msg: "History cleared".to_string(),
            },
        ]
    }

    fn reduce_history_cleared_with_notice(&mut self, notice: String) -> Vec<Effect> {
        let mut effects = self.reduce_history_cleared();
        #[cfg(feature = "terminal-tui")]
        {
            self.ui
                .conversation_lines
                .push(crate::chat::tui::ConversationLine::System { content: notice });
        }
        #[cfg(not(feature = "terminal-tui"))]
        {
            let _ = notice;
        }
        effects.push(Effect::RequestRedraw);
        effects
    }

    /// `Action::HistoryCompacted` — 对 LLM context history 做 compaction.
    ///
    /// 算法与 `chat::mod::compact_chat_history` 完全对齐（双写期两路径必须产生
    /// 字节级相同结果）:
    /// 1. `history.len() <= 1` 时直接返回（无可压缩 turn）.
    /// 2. 保留 system prompt（首位若 role==system）.
    /// 3. 只保留最后 [`COMPACT_KEEP_MESSAGES`] 条非 system 消息（drain 较老者）.
    /// 4. 单条消息超 [`COMPACT_CONTENT_CHARS`] 字符时用 ellipsis 截断.
    /// 5. 总预算超 [`COMPACT_TOTAL_CHARS`] 时按 FIFO drop oldest turn.
    fn reduce_history_compacted(&mut self, reason: CompactReason) -> Vec<Effect> {
        let history = &mut self.session.history;
        if history.len() <= 1 {
            return vec![Effect::LogTrace {
                level: tracing::Level::DEBUG,
                msg: format!("HistoryCompacted noop reason={reason:?} len={}", history.len()),
            }];
        }
        compact_history_in_place(history);

        let final_chars: usize = history.iter().map(|m| m.content.chars().count()).sum();
        vec![Effect::LogTrace {
            level: tracing::Level::INFO,
            msg: format!(
                "HistoryCompacted reason={reason:?} len={} chars={final_chars}",
                history.len()
            ),
        }]
    }

    fn reduce_history_compaction_patch_applied(
        &mut self,
        reason: CompactReason,
        patch: crate::agent::loop_::CompactionPatch,
        compaction_config: &crate::config::AgentCompactionConfig,
    ) -> Vec<Effect> {
        let history = &mut self.session.history;
        if crate::agent::loop_::compaction_patch_guard_matches(history, &patch.guard) {
            crate::agent::loop_::apply_compaction_patch_exact(history, &patch);
            let budget = crate::agent::loop_::plan_context_budget(
                history,
                compaction_config,
                crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD,
            );
            let trim_fallback = if budget.over_hard_limit {
                crate::agent::loop_::trim_history_to_context_budget_preserving_compaction_replacement_with_floor(
                    history,
                    compaction_config,
                    patch.replacement.len(),
                )
            } else {
                false
            };
            self.session.turns = durable_turns_from_compacted_history(history);
            self.ui.conversation_lines = conversation_lines_from_turns(&self.session.turns);
            self.ui.conversation_generation = self.ui.conversation_generation.saturating_add(1);
            self.ui.turn_count = self.session.turns.len();

            return vec![
                Effect::LogTrace {
                    level: tracing::Level::INFO,
                    msg: format!(
                        "HistoryCompactionPatchApplied reason={reason:?} start={} end={} replacement={} append_after={} trim_fallback={} len={}",
                        patch.range_start,
                        patch.range_end,
                        patch.replacement.len(),
                        patch.append_after.len(),
                        trim_fallback,
                        history.len()
                    ),
                },
                Effect::SaveSession(self.build_session_snapshot()),
                Effect::RequestRedraw,
            ];
        }

        let before_len = history.len();
        let trimmed = crate::agent::loop_::trim_history_to_context_budget(history, compaction_config);
        vec![Effect::LogTrace {
            level: tracing::Level::WARN,
            msg: format!(
                "HistoryCompactionPatch guard mismatch reason={reason:?}; stale patch ignored; trim_fallback={trimmed} before_len={before_len} after_len={}",
                history.len()
            ),
        }]
    }

    /// 辅助：从 StreamState 中取出当前 draft 的 id（不同 feature 下结构不同）.
    #[cfg(feature = "terminal-tui")]
    fn take_draft_id(stream: &StreamState) -> Option<String> {
        stream.primary_streaming_draft().map(|d| d.draft_id.clone())
    }

    #[cfg(not(feature = "terminal-tui"))]
    fn take_draft_id(stream: &StreamState) -> Option<String> {
        stream.primary_streaming_draft().map(|d| d.draft_id.clone())
    }
}

// ─── Public helpers (shared with dispatcher driver) ──────────────────────────

/// **S3 T3-1**: 与 `Action::HistoryCompacted` reducer 共享的 history 压缩算法.
///
/// 抽到 free function 是为了让 `dispatcher::drive_start_turn_stream` 在 context-overflow
/// 重试路径也能对自己持有的 `history` 副本应用**同一**算法，避免 reducer/driver 两侧
/// 状态漂移（Codex 审计建议）。
///
/// 行为与 `reduce_history_compacted` 完全一致：
/// 1. 保留 system prompt（首位若 role==system）.
/// 2. 只保留最后 [`COMPACT_KEEP_MESSAGES`] 条非 system 消息（drain 较老者）.
/// 3. 单条消息超 [`COMPACT_CONTENT_CHARS`] 字符时 ellipsis 截断.
/// 4. 总预算超 [`COMPACT_TOTAL_CHARS`] 时按 FIFO drop oldest turn.
///
/// `history.len() <= 1` 时为 no-op（保持 system 唯一消息或全空）。
pub fn compact_history_in_place(history: &mut Vec<ChatMessage>) {
    if history.len() <= 1 {
        return;
    }
    let has_system = history.first().is_some_and(|m| m.role == "system");
    let start = usize::from(has_system);

    // Step 1: 只保留最后 COMPACT_KEEP_MESSAGES 条非 system 消息
    let turn_count = history.len().saturating_sub(start);
    if turn_count > COMPACT_KEEP_MESSAGES {
        let drain_end = start.saturating_add(turn_count.saturating_sub(COMPACT_KEEP_MESSAGES));
        history.drain(start..drain_end);
    }

    // Step 2: 单条消息内容截断
    for msg in history.iter_mut().skip(start) {
        if msg.content.chars().count() > COMPACT_CONTENT_CHARS {
            msg.content = truncate_with_ellipsis(&msg.content, COMPACT_CONTENT_CHARS);
        }
    }

    // Step 3: 总预算约束（drop oldest first）
    while history
        .iter()
        .skip(start)
        .map(|m| m.content.chars().count())
        .sum::<usize>()
        > COMPACT_TOTAL_CHARS
        && history.len() > start.saturating_add(1)
    {
        history.remove(start);
    }
}

// ─── Internal helpers ────────────────────────────────────────────────────────

/// S4-A Commit 1: 静态判断给定 [`Action`] 是否影响 UI 渲染所需的字段
/// (`ui.conversation_lines` / `stream.draft` / `ui.input`).
///
/// **exhaustive match** 保证未来新增 Action 变体编译期可见漏写：
/// 编译器若发现新 variant 未匹配，cargo check 直接报错。
///
/// dirty=true 的 Action：reducer 调用后必产生 UI 字段变化，dispatcher 应
/// 构造新 [`UiSnapshot`] 推送给 watch。
///
/// dirty=false 的 Action：reducer 仅写入 session/control 子状态或仅产生
/// LogTrace，无需触发 watch send_if_modified。
///
/// **运行时兜底**：`reduce_tracked` 在静态判定为 false 时再用
/// [`ChatState::snapshot_dirty_fields`] 比较 reduce 前后指纹，捕捉静态白名单
/// 未明示的边缘情况（如某些 KeyPressed 实际未触发 input 变化但 reducer 走
/// 路径未变 — 静态返回 true 也无害，运行时 send_if_modified 会跳过相同帧）.
#[cfg(feature = "terminal-tui")]
const fn ui_dirty_for(action: &Action) -> bool {
    match action {
        // 输入路径：写 ui.input → dirty
        Action::KeyPressed(_)
        | Action::PasteReceived(_)
        | Action::InputSubmitted(_)
        | Action::InputReplaced(_)
        | Action::HistoryNavigated(_)
        | Action::InputCancelled => true,

        // 终端尺寸变化不影响 snapshot 字段集，redraw 经 Effect::RequestRedraw → redraw_tx 走
        Action::TerminalResized { .. } => false,

        // UI 折叠/展开：直接 mutate conversation_lines → dirty
        Action::ToolCardFoldToggled | Action::ReasoningFoldToggled => true,

        // 槽命令本身 reducer 是 no-op（实际执行在 mod.rs），不变 UI
        Action::SlashCommandIssued { .. } => false,
        // 模式切换：status bar 显示 mode 字段.
        Action::ModeChanged(_) => true,
        // BUG-07: 模型切换写 session.model，status bar 显示该字段 → dirty.
        Action::ModelChanged { .. } => true,
        // Bug #3: provider 切换写 session.provider（status bar 显示该字段）→ dirty.
        Action::ProviderChanged { .. } => true,

        // 流式 / 工具事件：全部写 stream.draft 或 conversation_lines → dirty
        Action::TurnStarted { .. }
        | Action::StartLLMTurn { .. }
        | Action::StreamChunkReceived { .. }
        | Action::StreamCompleted { .. }
        | Action::StreamFailed { .. }
        | Action::StreamCancelled { .. }
        | Action::ToolStarted { .. }
        | Action::ToolFinished { .. } => true,
        // 仅 LogTrace，不变 UI
        Action::StreamRetryAttempt { .. }
        | Action::StreamUsageMetered { .. }
        | Action::ProviderTurnReadyForCommit { .. } => false,
        Action::ToolProgress { .. } => true,
        // Foreground approval writes pending view + focus.
        Action::ToolApprovalRequested { .. } | Action::ToolApprovalReceived { .. } | Action::ToolApprovalCleared => {
            true
        }

        // 会话：SessionLoaded 重建 history + 可能要求 UI 重置；SessionSaved/Switched 不影响 UI
        Action::SessionLoaded(_) => true,
        Action::SessionSaved { .. } | Action::SessionSwitched { .. } => false,
        // Record*/compaction writes session.turns/history only. User-visible
        // compaction feedback is `SystemMessageAdded`; budget UI refresh is
        // `ContextWindowUpdated`, so the history patch itself is not snapshot-dirty.
        Action::RecordUserTurn(_)
        | Action::RecordAssistantTurn { .. }
        | Action::RecordSystemMessage { .. }
        | Action::SetLeadingSystemPrompt { .. }
        | Action::HistoryCompacted { .. }
        | Action::HistoryCompactionPatchApplied { .. } => false,
        // v4: BackgroundSessionRecorded only upserts session.background_sessions
        // (a persistence field, not a snapshot/UI field) → no UI dirty.
        Action::BackgroundSessionRecorded { .. } => false,

        // UI 镜像账本 / 历史清空 / Pure 模式用户 echo：直接动 conversation_lines → dirty
        Action::SystemMessageAdded { .. }
        | Action::HistoryCleared
        | Action::HistoryClearedWithNotice { .. }
        | Action::UserMessageEchoed(_) => true,
        // v1b/P1: writes sessions snapshot fields → dirty. The main loop only
        // dispatches these when content changes, and reducers also no-op
        // identical writes, so this never churns frames.
        Action::SessionsStatusUpdated { .. }
        | Action::SessionsEntriesUpdated { .. }
        | Action::MainQueueStatusUpdated { .. }
        | Action::ProviderWorkerStatusUpdated { .. }
        | Action::SlashMenuSourcesUpdated { .. }
        | Action::AtPathCandidatesUpdated { .. }
        | Action::ActiveSessionViewUpdated { .. }
        | Action::ContextWindowUpdated { .. }
        | Action::ProviderUsageRecorded { .. } => true,
        // v1.1b: focus + switcher are snapshot fields driving the prompt
        // indicator and switcher overlay → dirty. Each reducer no-ops identical
        // writes so unchanged state never churns frames.
        Action::SessionFocusChanged { .. }
        | Action::SwitcherOpened { .. }
        | Action::SwitcherMoved { .. }
        | Action::SwitcherClosed
        | Action::StripSelectionChanged { .. }
        | Action::SavedSessionPickerOpened { .. }
        | Action::SavedSessionPickerMoved { .. }
        | Action::SavedSessionPickerClosed => true,
        // RedrawRequested 仅产生 RequestRedraw Effect，本身不变 snapshot 字段；
        // 但语义上需要触发 redraw — 标 dirty 走 watch 路径.
        Action::RedrawRequested => true,

        // 退出：CancelRequested / targeted provider cancel / ShutdownRequested
        // may clear visible drafts.
        Action::CancelRequested | Action::CancelProviderTurn { .. } | Action::ShutdownRequested => true,
        // ForceQuit 仅发 Quit Effect，UI 立刻被 unmount，dirty 无意义.
        Action::ForceQuit => false,
    }
}

/// 双击 Ctrl+C 退出窗口（毫秒）.
const DOUBLE_CTRLC_WINDOW_MS: u64 = 500;

/// 读取当前墙钟（ms 自 UNIX epoch）。reducer 内唯一允许的"非纯"调用 —
/// 仅用于 Ctrl+C 双击窗口判断。测试通过 [`ChatState::reduce_with_now`] 注入。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> ChatState {
        let shutdown = CancellationToken::new();
        ChatState::new(Arc::from("test-provider"), Arc::from("test-model"), shutdown)
    }

    /// 验证 SessionState 默认值是否合理
    #[test]
    fn test_chatstate_new_default_session() {
        let state = make_state();
        assert!(uuid::Uuid::parse_str(&state.session.id).is_ok());
        assert!(state.session.title.is_empty());
        assert_eq!(&*state.session.provider, "test-provider");
        assert_eq!(&*state.session.model, "test-model");
        assert!(state.session.turns.is_empty());
        assert!(state.session.history.is_empty());
    }

    /// 验证 UiState 默认值是否合理
    #[test]
    fn test_chatstate_new_default_ui() {
        let state = make_state();
        assert!(state.ui.conversation_lines.is_empty());
        assert!(state.ui.input.is_empty());
        assert_eq!(state.ui.turn_count, 0);
        assert!(!state.ui.ascii_fallback);
        assert_eq!(state.ui.last_ctrlc_ms, 0);
    }

    /// 验证 StreamState 默认值是否合理
    #[test]
    fn test_chatstate_new_default_stream() {
        let state = make_state();
        assert!(state.stream.primary_streaming_draft().is_none());
        assert!(state.control.tool_buffers.is_empty());
    }

    /// v1b: SessionsStatusUpdated 写入 ui.sessions_status 并经快照反映；相同内容 no-op.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn sessions_status_updated_writes_and_dedups() {
        let mut state = make_state();
        assert!(state.ui.sessions_status.is_empty());

        let effects = state.reduce(Action::SessionsStatusUpdated {
            summary: "sessions: 1 running".to_string(),
        });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.sessions_status, "sessions: 1 running");
        let snap = state.build_ui_snapshot(1);
        assert_eq!(&*snap.sessions_status, "sessions: 1 running");

        // Identical write is a no-op (no redraw effect).
        let effects = state.reduce(Action::SessionsStatusUpdated {
            summary: "sessions: 1 running".to_string(),
        });
        assert!(effects.is_empty(), "identical status must not emit an effect");

        // Clearing hides the row (empty string flows through).
        let effects = state.reduce(Action::SessionsStatusUpdated { summary: String::new() });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(state.ui.sessions_status.is_empty());
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn main_queue_status_updated_writes_snapshot_and_dedups() {
        let mut state = make_state();
        let status = MainQueueStatus { queued: 3, priority: 1 };

        let effects = state.reduce(Action::MainQueueStatusUpdated { status });

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.main_queue_status, status);
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.main_queue_status, status);

        let effects = state.reduce(Action::MainQueueStatusUpdated { status });
        assert!(effects.is_empty(), "identical queue status must not emit an effect");
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn provider_worker_status_updated_writes_snapshot_and_dedups() {
        let mut state = make_state();
        let status = ProviderWorkerStatus {
            running: 1,
            cancelling: 1,
            awaiting_commit: 2,
            finalized_payloads: 0,
            finalized_total_tokens: 0,
            oldest_started_at_ms: None,
            rows: Vec::new(),
        };

        let effects = state.reduce(Action::ProviderWorkerStatusUpdated { status: status.clone() });

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.provider_worker_status, status);
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.provider_worker_status, status);

        let effects = state.reduce(Action::ProviderWorkerStatusUpdated { status });
        assert!(
            effects.is_empty(),
            "identical provider worker status must not emit an effect"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn provider_worker_status_update_refreshes_open_worker_view_with_io() {
        use crate::chat::tui::{ConversationLine, ToolStatus};

        let mut state = make_state();
        state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 2 };
        state.ui.conversation_lines.push(ConversationLine::ToolResult {
            tool_name: "shell".to_string(),
            args_preview: "echo P6Z".to_string(),
            args_full: "{\"command\":\"echo P6Z\"}".to_string(),
            result: Some("P6Z\n".to_string()),
            status: ToolStatus::Done,
            elapsed_ms: Some(12),
            folded: true,
        });
        let status = ProviderWorkerStatus {
            running: 1,
            cancelling: 0,
            awaiting_commit: 0,
            finalized_payloads: 0,
            finalized_total_tokens: 0,
            oldest_started_at_ms: Some(chrono::Utc::now().timestamp_millis()),
            rows: vec![crate::chat::action::ProviderWorkerStatusRow {
                task_id: 7,
                sequence: 2,
                kind: crate::chat::action::ProviderWorkerRowKind::ForegroundAwaited,
                state: crate::chat::action::ProviderWorkerRowState::Running,
                started_at_ms: chrono::Utc::now().timestamp_millis(),
                finalized_total_tokens: None,
                completion_ready: false,
            }],
        };

        let effects = state.reduce(Action::ProviderWorkerStatusUpdated { status });

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        let view = state.ui.active_session_view.as_ref().expect("worker view refreshed");
        assert_eq!(view.kind, crate::chat::action::PROVIDER_WORKER_VIEW_KIND);
        assert!(view.lines.iter().any(|line| line == "task: 7"));
        assert!(
            !view.lines.iter().any(|line| line == "io: recent provider turn"),
            "non-streaming worker views must not replay transcript history: {:?}",
            view.lines
        );
        assert!(
            !view.lines.iter().any(|line| line.starts_with("run shell done:")),
            "completed tool cards without a matching streaming draft stay out of worker IO: {:?}",
            view.lines
        );
        assert!(
            !view.lines.iter().any(|line| line == "output: P6Z"),
            "completed tool output without a matching streaming draft stays out of worker IO: {:?}",
            view.lines
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn slash_menu_sources_match_legacy_and_redux_for_same_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let catalog = vec![crate::chat::tui::SlashProviderModelCatalog {
            provider: "test-provider".to_string(),
            models: vec![crate::chat::tui::SlashModelCandidate {
                name: "gpt-parity".to_string(),
                description: "Parity model".to_string(),
            }],
        }];
        let mut legacy = crate::chat::tui::TuiState::new("test-provider", "test-model");
        legacy.provider_model_catalog = catalog.clone();
        let mut redux = make_state();
        let _ = redux.reduce(Action::SlashMenuSourcesUpdated {
            saved_sessions: Vec::new(),
            provider_model_catalog: catalog,
        });

        for ch in "/model ".chars() {
            let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
            let _ = crate::chat::tui::dispatch_global_key(key, &mut legacy);
            let _ = redux.reduce_with_now(Action::KeyPressed(key), 1_000);
        }

        let legacy_labels = legacy
            .slash_menu
            .as_ref()
            .expect("legacy model menu")
            .entries
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>();
        let redux_labels = redux
            .ui
            .slash_menu
            .as_ref()
            .expect("redux model menu")
            .entries
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(legacy_labels, redux_labels);
        assert_eq!(redux_labels, vec!["gpt-parity"]);
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn at_path_candidates_action_opens_redux_menu_and_tab_inserts() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        let _ = state.reduce(Action::InputReplaced("inspect @ca".to_string()));
        assert!(
            state.ui.slash_menu.is_none(),
            "input alone has no candidates and must not render a stale menu"
        );

        let effects = state.reduce(Action::AtPathCandidatesUpdated {
            candidates: vec![AtPathCandidate {
                path: "Cargo.toml".to_string(),
                is_dir: false,
            }],
        });

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(
            state
                .ui
                .slash_menu
                .as_ref()
                .expect("@path menu")
                .entries
                .first()
                .map(|entry| entry.label.as_str()),
            Some("Cargo.toml")
        );
        let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert_eq!(state.ui.input.text(), "inspect @Cargo.toml ");
    }

    /// P1: SessionsEntriesUpdated writes structured strip entries and dedups
    /// identical snapshots so the 1s poll does not churn redraws.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn sessions_entries_updated_writes_snapshot_and_dedups() {
        use crate::chat::sessions::SwitcherEntry;
        let mut state = make_state();
        assert!(state.ui.sessions_entries.is_empty());

        let entries = vec![SwitcherEntry {
            seq: 1,
            kind: "agent",
            origin: "user",
            status: "running",
            title: "task".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        }];
        let expected = entries.clone();
        let effects = state.reduce(Action::SessionsEntriesUpdated { entries });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.sessions_entries, expected);
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.sessions_entries.as_slice(), expected.as_slice());

        let effects = state.reduce(Action::SessionsEntriesUpdated { entries: expected });
        assert!(effects.is_empty(), "identical entries must not redraw");

        let effects = state.reduce(Action::SessionsEntriesUpdated { entries: Vec::new() });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(state.ui.sessions_entries.is_empty());
    }

    /// P2: ActiveSessionViewUpdated writes the focused child viewport snapshot,
    /// flows it to UiSnapshot, and dedups identical writes.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn active_session_view_updated_writes_snapshot_and_dedups() {
        let mut state = make_state();
        assert!(state.ui.active_session_view.is_none());

        let view = crate::chat::sessions::ActiveSessionView {
            seq: 4,
            kind: "shell".to_string(),
            title: "tail -f app.log".to_string(),
            lines: vec!["a".to_string(), "b".to_string()],
            truncated: false,
            scroll_offset: 1,
        };
        let effects = state.reduce(Action::ActiveSessionViewUpdated {
            view: Some(view.clone()),
        });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.active_session_view, Some(view.clone()));
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.active_session_view, Some(view));

        let duplicate = state.ui.active_session_view.clone();
        let effects = state.reduce(Action::ActiveSessionViewUpdated { view: duplicate });
        assert!(effects.is_empty(), "identical active view must not redraw");

        let effects = state.reduce(Action::ActiveSessionViewUpdated { view: None });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(state.ui.active_session_view.is_none());
    }

    /// P4c: ContextWindowUpdated writes status-bar window metadata to both the
    /// live UI state and UiSnapshot, with identical writes deduped.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn context_window_updated_writes_snapshot_and_dedups() {
        let mut state = make_state();
        assert_eq!(state.ui.context_used_tokens, None);
        assert_eq!(state.ui.context_window_tokens, None);

        let effects = state.reduce(Action::ContextWindowUpdated {
            used_context_tokens: Some(2_500),
            max_context_tokens: Some(10_000_000),
        });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.context_used_tokens, Some(2_500));
        assert_eq!(state.ui.context_window_tokens, Some(10_000_000));
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.context_used_tokens, Some(2_500));
        assert_eq!(snap.context_window_tokens, Some(10_000_000));

        let effects = state.reduce(Action::ContextWindowUpdated {
            used_context_tokens: Some(2_500),
            max_context_tokens: Some(10_000_000),
        });
        assert!(effects.is_empty(), "identical context budget must not redraw");

        let effects = state.reduce(Action::ContextWindowUpdated {
            used_context_tokens: None,
            max_context_tokens: None,
        });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.context_used_tokens, None);
        assert_eq!(state.ui.context_window_tokens, None);
    }

    /// v1.1b: SessionFocusChanged writes ui.focus + flows to the snapshot; an
    /// identical focus is a no-op.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn session_focus_changed_writes_and_dedups() {
        use crate::chat::sessions::FocusTarget;
        let mut state = make_state();
        assert_eq!(state.ui.focus, FocusTarget::Main);

        let focus = FocusTarget::Session { seq: 2 };
        let effects = state.reduce(Action::SessionFocusChanged { focus });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.focus, focus);
        let snap = state.build_ui_snapshot(1);
        assert_eq!(snap.focus, focus);

        // Identical focus → no-op.
        let effects = state.reduce(Action::SessionFocusChanged { focus });
        assert!(effects.is_empty(), "identical focus must not emit an effect");

        // Back to main.
        let effects = state.reduce(Action::SessionFocusChanged {
            focus: FocusTarget::Main,
        });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.focus, FocusTarget::Main);
    }

    /// v1.1b: switcher open/move/close lifecycle through the reducer.
    #[cfg(feature = "terminal-tui")]
    #[test]
    fn switcher_open_move_close_lifecycle() {
        use crate::chat::sessions::SwitcherEntry;
        let mut state = make_state();
        assert!(state.ui.switcher.is_none());

        let entries = vec![
            SwitcherEntry {
                seq: 1,
                kind: "agent",
                origin: "user",
                status: "running",
                title: "a".into(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            },
            SwitcherEntry {
                seq: 2,
                kind: "agent",
                origin: "model",
                status: "completed",
                title: "b".into(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            },
        ];
        let effects = state.reduce(Action::SwitcherOpened { entries });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        let sw = state.ui.switcher.as_ref().expect("test: switcher open");
        assert_eq!(sw.len(), 2);
        assert_eq!(sw.selected, 0);
        // Snapshot carries it.
        assert!(state.build_ui_snapshot(1).switcher.is_some());

        // Move to row 1.
        let effects = state.reduce(Action::SwitcherMoved { selected: 1 });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.switcher.as_ref().expect("test").selected, 1);
        // Out-of-range selection is clamped to the last row, not a panic.
        let _ = state.reduce(Action::SwitcherMoved { selected: 99 });
        assert_eq!(state.ui.switcher.as_ref().expect("test").selected, 1);

        // Close.
        let effects = state.reduce(Action::SwitcherClosed);
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(state.ui.switcher.is_none());
        // Closing again is a no-op.
        let effects = state.reduce(Action::SwitcherClosed);
        assert!(effects.is_empty());
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn strip_selection_changed_writes_snapshot_without_focus_change() {
        let mut state = make_state();
        state.ui.focus = crate::chat::sessions::FocusTarget::Session { seq: 1 };

        let effects = state.reduce(Action::StripSelectionChanged { selected: Some(2) });

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.strip_selection, Some(2));
        assert_eq!(
            state.ui.focus,
            crate::chat::sessions::FocusTarget::Session { seq: 1 },
            "strip selection is not input routing focus"
        );
        assert_eq!(state.build_ui_snapshot(1).strip_selection, Some(2));

        let effects = state.reduce(Action::StripSelectionChanged { selected: Some(2) });
        assert!(effects.is_empty(), "unchanged strip selection is a reducer no-op");

        let effects = state.reduce(Action::StripSelectionChanged { selected: None });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.strip_selection, None);
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn alt_enter_stale_strip_selection_clears_and_surfaces_session_gone() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        state.ui.sessions_entries = vec![crate::chat::sessions::SwitcherEntry {
            seq: 1,
            kind: "agent",
            origin: "user",
            status: "running",
            title: "task".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        }];
        state.ui.strip_selection = Some(2);
        state.ui.input.set_text("draft");

        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.strip_selection, None);
        assert_eq!(
            state.ui.input.text(),
            "draft",
            "stale Alt+Enter must not insert a newline"
        );
        assert!(
            matches!(
                state.ui.conversation_lines.last(),
                Some(crate::chat::tui::ConversationLine::System { content }) if content == "session gone"
            ),
            "reducer should surface session gone"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn alt_enter_matching_strip_selection_uses_attach_branch_without_newline() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        state.ui.sessions_entries = vec![crate::chat::sessions::SwitcherEntry {
            seq: 2,
            kind: "shell",
            origin: "user",
            status: "running",
            title: "task".into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            token_usage_records: Vec::new(),
            idle_warning: false,
        }];
        state.ui.strip_selection = Some(2);
        state.ui.input.set_text("draft");

        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));

        assert!(
            matches!(
                effects.as_slice(),
                [Effect::LogTrace { msg, .. }, Effect::RequestRedraw] if msg.contains("strip_alt_enter_attach seq=2")
            ),
            "matching Alt+Enter should take attach branch: {effects:?}"
        );
        assert_eq!(state.ui.strip_selection, Some(2));
        assert_eq!(
            state.ui.input.text(),
            "draft",
            "matching Alt+Enter must not insert a newline"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn alt_enter_without_strip_selection_falls_through_to_newline_insert() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        state.ui.input.set_text("a");

        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::NONE,
        )));

        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.input.text(), "a\nb");
        assert!(
            state.ui.conversation_lines.is_empty(),
            "no strip selection means Alt+Enter falls through to input, not session gone"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn slash_menu_captures_alt_arrows_before_strip_selection() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        state.ui.sessions_entries = vec![
            crate::chat::sessions::SwitcherEntry {
                seq: 1,
                kind: "agent",
                origin: "user",
                status: "running",
                title: "one".into(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            },
            crate::chat::sessions::SwitcherEntry {
                seq: 2,
                kind: "agent",
                origin: "user",
                status: "running",
                title: "two".into(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            },
        ];
        state.ui.input.set_text("/mo");
        state.ui.slash_menu = Some(SlashMenuState::new("mo"));
        state.ui.strip_selection = Some(2);

        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)));

        assert!(
            effects.iter().all(|effect| matches!(effect, Effect::RequestRedraw)),
            "slash menu may redraw, but must not leak Alt+Up into strip navigation: {effects:?}"
        );
        assert_eq!(
            state.ui.strip_selection,
            Some(2),
            "Alt+Up must not move the session strip while slash menu is open"
        );
        assert!(state.ui.slash_menu.is_some(), "slash menu remains open");
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn saved_session_picker_captures_alt_enter_before_stale_strip_selection() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut state = make_state();
        state.ui.saved_session_picker = Some(crate::chat::session::SavedSessionPickerState::new(vec![
            crate::chat::session::SavedSessionPickerEntry {
                id: "saved-a".to_string(),
                title: "saved a".to_string(),
                turn_count: 1,
                updated_at: chrono::Utc::now(),
                provider: "p".to_string(),
                model: "m".to_string(),
                is_current: false,
            },
        ]));
        state.ui.strip_selection = Some(99);
        state.ui.input.set_text("draft");

        let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));

        assert!(
            effects.is_empty(),
            "saved picker consumes Alt+Enter without reducer side effects"
        );
        assert!(
            state.ui.saved_session_picker.is_some(),
            "Alt+Enter is consumed, not treated as picker Enter"
        );
        assert_eq!(state.ui.strip_selection, Some(99));
        assert_eq!(state.ui.input.text(), "draft");
        assert!(
            !matches!(
                state.ui.conversation_lines.last(),
                Some(crate::chat::tui::ConversationLine::System { content }) if content == "session gone"
            ),
            "stale strip selection must not receive Alt+Enter while picker is open"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn saved_session_picker_open_move_close_lifecycle() {
        let mut state = make_state();
        state.ui.switcher = Some(crate::chat::sessions::SwitcherState::new(vec![
            crate::chat::sessions::SwitcherEntry {
                seq: 1,
                kind: "agent",
                origin: "model",
                status: "running",
                title: "child".to_string(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                token_usage_records: Vec::new(),
                idle_warning: false,
            },
        ]));
        let entries = vec![
            crate::chat::session::SavedSessionPickerEntry {
                id: "saved-a".to_string(),
                title: "saved a".to_string(),
                turn_count: 2,
                updated_at: chrono::Utc::now(),
                provider: "p".to_string(),
                model: "m".to_string(),
                is_current: true,
            },
            crate::chat::session::SavedSessionPickerEntry {
                id: "saved-b".to_string(),
                title: "saved b".to_string(),
                turn_count: 4,
                updated_at: chrono::Utc::now(),
                provider: "p".to_string(),
                model: "m".to_string(),
                is_current: false,
            },
        ];

        let effects = state.reduce(Action::SavedSessionPickerOpened { entries });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(
            state.ui.switcher.is_none(),
            "saved picker and child switcher are mutually exclusive"
        );
        let picker = state.ui.saved_session_picker.as_ref().expect("picker open");
        assert_eq!(picker.len(), 2);
        assert_eq!(picker.selected, 0);
        assert_eq!(
            state
                .build_ui_snapshot(1)
                .saved_session_picker
                .as_ref()
                .expect("snapshot picker")
                .entries
                .len(),
            2
        );

        let effects = state.reduce(Action::SavedSessionPickerMoved { selected: 99 });
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert_eq!(state.ui.saved_session_picker.as_ref().expect("picker").selected, 1);
        let effects = state.reduce(Action::SavedSessionPickerMoved { selected: 1 });
        assert!(effects.is_empty(), "same clamped selection is no-op");

        let effects = state.reduce(Action::SavedSessionPickerClosed);
        assert!(matches!(effects.as_slice(), [Effect::RequestRedraw]));
        assert!(state.ui.saved_session_picker.is_none());
        assert!(state.reduce(Action::SavedSessionPickerClosed).is_empty());
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn tool_approval_requested_opens_approval_child_view() {
        let mut state = make_state();
        let effects = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-approve".to_string(),
            name: "shell".to_string(),
            args: r#"{"cmd":"printf secure"}"#.to_string(),
        });
        assert!(state.ui.pending_tool_approval.is_some());
        assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Approval);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::RequestApproval { .. }))
        );

        let snap = state.build_ui_snapshot(1);
        let pending = snap
            .pending_tool_approval
            .as_ref()
            .expect("pending approval in snapshot");
        assert_eq!(pending.tool_id, "call-approve");
        assert_eq!(pending.name, "shell");
        assert!(pending.args.contains("printf secure"));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn tool_approval_received_closes_approval_child_view() {
        let mut state = make_state();
        let _ = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-deny".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        });
        let effects = state.reduce(Action::ToolApprovalReceived {
            tool_id: "call-deny".to_string(),
            approved: false,
        });
        assert!(state.ui.pending_tool_approval.is_none());
        assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
        assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn redux_esc_approval_generating_denies_without_cancelling_turn() {
        let mut state = make_state();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-approval".to_string(),
            cancel: CancellationToken::new(),
        });
        let _ = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-esc-deny".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        });

        let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        )));

        assert!(
            state.control.generating,
            "Esc in approval must not cancel the active turn"
        );
        assert!(
            state.stream.primary_streaming_draft().is_some(),
            "streaming draft stays active"
        );
        assert!(state.ui.pending_tool_approval.is_none());
        assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::ResolveApproval { tool_id, approved: false } if tool_id == "call-esc-deny"
            )
        }));
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::CancelToken(_) | Effect::CancelDraft(_))),
            "approval Esc must not emit turn-cancel effects"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn redux_esc_generating_slash_menu_closes_menu_without_cancelling_turn() {
        let mut state = make_state();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-slash".to_string(),
            cancel: CancellationToken::new(),
        });
        state.ui.input.set_text("/mo");
        state.ui.slash_menu = Some(SlashMenuState::new("mo"));

        let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        )));

        assert!(state.control.generating, "slash Esc must not cancel the active turn");
        assert!(
            state.stream.primary_streaming_draft().is_some(),
            "streaming draft stays active"
        );
        assert!(state.ui.slash_menu.is_none(), "Esc closes only the slash menu");
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::CancelToken(_) | Effect::CancelDraft(_))),
            "slash Esc must not emit turn-cancel effects"
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn cancel_requested_clears_pending_approval_and_resolves_false() {
        let mut state = make_state();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-cancel".to_string(),
            cancel: CancellationToken::new(),
        });
        let _ = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-cancel-deny".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        });

        let effects = state.reduce(Action::CancelRequested);

        assert!(state.ui.pending_tool_approval.is_none());
        assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
        assert!(effects.iter().any(|effect| matches!(effect, Effect::CancelToken(_))));
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::ResolveApproval { tool_id, approved: false } if tool_id == "call-cancel-deny"
            )
        }));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::CancelDraft(draft_id) if draft_id == "draft-cancel"))
        );
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn tool_approval_focus_paste_does_not_edit_input() {
        let mut state = make_state();
        let _ = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-paste".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        });
        let effects = state.reduce(Action::PasteReceived("must not enter input".to_string()));
        assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
        assert!(state.ui.input.is_empty());
        assert!(state.ui.pending_tool_approval.is_some());
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn session_loaded_resets_transient_holder_set() {
        let mut state = make_state();
        let _ = state.reduce(Action::ToolApprovalRequested {
            task_id: None,
            tool_id: "call-stale".to_string(),
            name: "shell".to_string(),
            args: "{}".to_string(),
        });
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-stale".to_string(),
            cancel: CancellationToken::new(),
        });
        state.ui.context_window_tokens = Some(10_000_000);
        state.ui.context_used_tokens = Some(2_500);
        state.ui.input.set_text("draft text");
        assert!(state.ui.input.begin_or_cycle_reverse_search());
        state.control.generating = false;

        let mut loaded = ChatSession::new("prov-new", "model-new");
        loaded.id = "sess-new".to_string();
        loaded.add_user_turn("hello");
        loaded.add_assistant_turn("hi", vec![]);
        let effects = state.reduce(Action::SessionLoaded(loaded));

        assert!(state.ui.pending_tool_approval.is_none());
        assert_eq!(state.ui.focus, crate::chat::sessions::FocusTarget::Main);
        assert_eq!(state.ui.context_window_tokens, None);
        assert_eq!(state.ui.context_used_tokens, None);
        assert!(!state.ui.input.is_reverse_search_active());
        assert_eq!(state.ui.turn_count, 2);
        assert!(state.stream.primary_streaming_draft().is_none());
        assert!(!state.control.generating);
        assert!(state.control.active_cancel.is_none());
        assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn session_loaded_is_rejected_while_generating_without_clearing_active_turn() {
        let mut state = make_state();
        state.session.id = "sess-old".to_string();
        let cancel = CancellationToken::new();
        let _ = state.reduce(Action::TurnStarted {
            draft_id: "draft-active".to_string(),
            cancel: cancel.clone(),
        });
        assert!(state.control.generating);
        assert!(state.stream.primary_streaming_draft().is_some());
        assert!(state.control.active_cancel.is_some());

        let mut loaded = ChatSession::new("prov-new", "model-new");
        loaded.id = "sess-new".to_string();
        loaded.add_user_turn("should-not-load");
        let effects = state.reduce(Action::SessionLoaded(loaded));

        assert_eq!(state.session.id, "sess-old");
        assert!(state.control.generating);
        assert!(state.stream.primary_streaming_draft().is_some());
        assert!(state.control.active_cancel.is_some());
        assert!(!cancel.is_cancelled());
        assert!(!effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
        assert!(effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::LogTrace {
                    level: tracing::Level::WARN,
                    msg,
                } if msg.contains("SessionLoaded rejected while generating: sess-new")
            )
        }));
    }

    #[cfg(feature = "terminal-tui")]
    #[test]
    fn reducer_tab_mid_edit_inserts_tab_instead_of_folding() {
        let mut state = make_state();
        state.ui.input.set_text("alpha");

        let effects = state.reduce(Action::KeyPressed(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        )));

        assert_eq!(state.ui.input.text(), "alpha\t");
        assert!(effects.iter().any(|effect| matches!(effect, Effect::RequestRedraw)));
    }

    /// reduce 不 panic（健壮性 baseline，沿用 Step 1 名称便于 grep）
    #[test]
    fn test_reduce_key_pressed_returns_empty_step1() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut state = make_state();
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let _effects = state.reduce(Action::KeyPressed(key));
        // Step 2 起 KeyPressed('a') 会写入 input buffer → RequestRedraw
        // 此处仅断言不 panic
    }

    /// Step 4 完成后仍返回空的 Action（仅剩 SlashCommandIssued）
    #[test]
    fn test_reduce_unfilled_actions_return_empty() {
        let mut state = make_state();
        // Step 4 已填充: HistoryCleared/SessionLoaded/SessionSaved/SessionSwitched/
        //   RecordUserTurn/RecordAssistantTurn/CancelRequested/ShutdownRequested
        // 以下仍为 Step 5 实现（返回空 vec）:
        let unfilled = [Action::SlashCommandIssued {
            cmd: "clear".to_string(),
            args: String::new(),
        }];
        for action in unfilled {
            let effects = state.reduce(action);
            assert!(effects.is_empty(), "未填充 Action 应返回 vec![]");
        }
    }

    /// Step 3 新增：StreamChunkReceived 无 draft 时返回 vec![] (stale)
    #[test]
    fn test_reduce_stream_chunk_no_draft_returns_empty() {
        let mut state = make_state();
        let effects = state.reduce(Action::StreamChunkReceived {
            draft_id: "d1".to_string(),
            delta: "x".to_string(),
            version: 1,
        });
        assert!(effects.is_empty(), "无 draft 时 chunk 应丢弃");
    }

    /// 所有 Action 变体 reduce 不 panic（覆盖契约）
    #[test]
    fn test_reduce_does_not_panic_for_all_actions() {
        use crate::chat::action::HistoryDir;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
        let task_id = scheduler.enqueue("cancel target", crate::chat::turn_scheduler::TurnPriority::Normal, 0);

        let actions: Vec<Action> = vec![
            Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::PasteReceived("paste text".to_string()),
            Action::TerminalResized { w: 80, h: 24 },
            Action::InputSubmitted("hello".to_string()),
            Action::HistoryNavigated(HistoryDir::Up),
            Action::HistoryNavigated(HistoryDir::Down),
            Action::InputCancelled,
            Action::ToolCardFoldToggled,
            Action::ReasoningFoldToggled,
            Action::RedrawRequested,
            Action::CancelProviderTurn { task_id },
            Action::ForceQuit,
        ];

        for action in actions {
            let mut state = make_state();
            let _effects = state.reduce(action);
        }
    }

    /// Action 必须是 Send + Sync（编译期断言，保证可通过 channel 跨任务传递）
    #[test]
    fn test_action_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Action>();
    }

    /// Effect 必须是 Send + Sync（编译期断言，保证可通过 channel 传递给执行器）
    #[test]
    fn test_effect_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Effect>();
    }

    /// EmitChannelMessage 携带 SendMessage（P1 验证：不是裸 String）
    #[test]
    fn test_effect_emit_channel_message_has_send_message_type() {
        use crate::channels::traits::SendMessage;
        // 能构造 Effect::EmitChannelMessage(SendMessage) 说明类型正确接入
        let msg = SendMessage::new("hello", "bob");
        let effect = Effect::EmitChannelMessage(msg);
        // 验证 Debug 实现存在
        let debug_str = format!("{:?}", effect);
        assert!(
            debug_str.contains("EmitChannelMessage"),
            "EmitChannelMessage Debug 输出异常"
        );
    }

    /// ControlState 默认值验证
    #[test]
    fn test_chatstate_new_default_control() {
        let state = make_state();
        assert!(state.control.active_cancel.is_none());
        assert!(!state.control.generating);
    }

    /// 多次 reduce 调用不 panic（健壮性）
    #[test]
    fn test_reduce_multiple_calls_no_panic() {
        let mut state = make_state();
        for _ in 0..10 {
            let _effects = state.reduce(Action::RedrawRequested);
        }
    }

    // ─── Step 2 单元测试（输入路径） ───────────────────────────────────────────
    //
    // 大部分输入路径测试依赖 terminal-tui feature 提供的真实 TuiInput /
    // ConversationLine。非 TUI feature 下 reducer 走占位分支，行为退化为
    // "返回 RequestRedraw 且不 mutate buffer"，因此 Step 2 系列断言整体 cfg-gate。

    #[cfg(feature = "terminal-tui")]
    mod step2 {
        use super::super::*;
        use crate::chat::action::HistoryDir;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        fn s() -> ChatState {
            ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new())
        }

        fn has_request_redraw(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::RequestRedraw))
        }

        fn has_quit(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::Quit))
        }

        fn has_log_trace(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::LogTrace { .. }))
        }

        /// 1. Enter on non-empty buffer → 派生 InputSubmitted 路径，turn_count += 1
        #[test]
        fn test_reduce_key_pressed_enter_returns_input_submitted() {
            let mut state = s();
            // 模拟用户输入 "hi"
            for ch in "hi".chars() {
                let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
            }
            assert_eq!(state.ui.input.text(), "hi");
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
            assert!(has_log_trace(&effects), "Enter 应触发 LogTrace");
            assert!(has_request_redraw(&effects), "Enter 应触发 RequestRedraw");
            assert_eq!(state.ui.turn_count, 1, "turn_count 应递增");
            assert_eq!(state.ui.last_submitted.as_deref(), Some("hi"));
            assert!(state.ui.input.is_empty(), "提交后 buffer 应清空");
        }

        /// 2. Tab → ToolCardFoldToggled，返回 RequestRedraw
        #[test]
        fn test_reduce_key_pressed_tab_returns_fold_toggled() {
            let mut state = s();
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            assert!(has_request_redraw(&effects));
            // Tab 不应进入 input buffer
            assert!(state.ui.input.is_empty());
        }

        /// 3. Ctrl+C 单击 → 仅记录窗口，不返回 Quit
        #[test]
        fn test_reduce_key_pressed_ctrl_c_single_returns_cancel() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let effects = state.reduce_with_now(Action::KeyPressed(key), 10_000);
            assert!(!has_quit(&effects), "单击 Ctrl+C 不应 Quit");
            assert_eq!(state.ui.last_ctrlc_ms, 10_000, "记录窗口时间戳");
        }

        /// 4. Ctrl+C 500ms 内双击 → Quit
        #[test]
        fn test_reduce_key_pressed_ctrl_c_double_within_500ms_returns_shutdown() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let _ = state.reduce_with_now(Action::KeyPressed(key.clone()), 10_000);
            // 100ms 后再次 Ctrl+C
            let effects = state.reduce_with_now(Action::KeyPressed(key), 10_100);
            assert!(has_quit(&effects), "双击 Ctrl+C 应 Quit");
        }

        /// 5. Ctrl+C 超过 500ms 后再按 → 仅记录，不 Quit
        #[test]
        fn test_reduce_key_pressed_ctrl_c_double_after_500ms_returns_cancel_only() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let _ = state.reduce_with_now(Action::KeyPressed(key.clone()), 10_000);
            // 600ms 后 — 超过窗口
            let effects = state.reduce_with_now(Action::KeyPressed(key), 10_600);
            assert!(!has_quit(&effects), "超过 500ms 的双击不算双击");
            assert_eq!(state.ui.last_ctrlc_ms, 10_600);
        }

        /// 6. Ctrl+D on empty buffer → Quit
        #[test]
        fn test_reduce_key_pressed_ctrl_d_empty_buffer_returns_quit() {
            let mut state = s();
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )));
            assert!(has_quit(&effects), "空 buffer 上 Ctrl+D 应 Quit");
        }

        /// 7. Ctrl+D non-empty buffer → forward-delete，不 Quit
        #[test]
        fn test_reduce_key_pressed_ctrl_d_non_empty_buffer_inserts_char_or_eof() {
            let mut state = s();
            // 输入 "abc" 然后 Home 移到行首
            for ch in "abc".chars() {
                let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
            }
            let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
            assert_eq!(state.ui.input.text(), "abc");
            // Ctrl+D 应该 forward-delete 'a'
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )));
            assert!(!has_quit(&effects), "非空 buffer Ctrl+D 不 Quit");
            assert_eq!(state.ui.input.text(), "bc", "forward-delete 应删除 'a'");
        }

        /// 8. PasteReceived → 内容追加到 input buffer
        #[test]
        fn test_reduce_paste_received_appends_to_input() {
            let mut state = s();
            let effects = state.reduce(Action::PasteReceived("pasted-text".to_string()));
            assert!(has_request_redraw(&effects));
            assert_eq!(state.ui.input.text(), "pasted-text");
        }

        #[test]
        fn test_large_paste_is_bounded_before_submit() {
            let mut state = s();
            let line = "b".repeat(1024);
            let pasted = std::iter::repeat_n(line.as_str(), 100).collect::<Vec<_>>().join("\n");

            let effects = state.reduce(Action::PasteReceived(pasted.clone()));
            assert!(has_request_redraw(&effects));
            assert_eq!(state.ui.input.byte_len(), crate::chat::tui::INPUT_MAX_BYTES);
            assert!(state.ui.input.truncated);
            assert!(pasted.starts_with(&state.ui.input.text()));
            let bounded = state.ui.input.text();

            let effects = state.reduce(Action::InputSubmitted(state.ui.input.text()));
            assert!(has_request_redraw(&effects));
            assert!(has_log_trace(&effects));
            assert_eq!(state.ui.last_submitted.as_deref(), Some(bounded.as_str()));
            assert!(state.ui.input.is_empty());
        }

        /// 9. TerminalResized → RequestRedraw
        #[test]
        fn test_reduce_terminal_resized_returns_redraw() {
            let mut state = s();
            let effects = state.reduce(Action::TerminalResized { w: 120, h: 40 });
            assert!(has_request_redraw(&effects));
        }

        /// 10. InputSubmitted (直接) → turn_count 递增 + last_submitted 记录
        #[test]
        fn test_reduce_input_submitted_increments_turn_count() {
            let mut state = s();
            assert_eq!(state.ui.turn_count, 0);
            let effects = state.reduce(Action::InputSubmitted("hello world".to_string()));
            assert_eq!(state.ui.turn_count, 1);
            assert_eq!(state.ui.last_submitted.as_deref(), Some("hello world"));
            assert!(state.ui.input.is_empty());
            assert!(has_log_trace(&effects));
            assert!(has_request_redraw(&effects));
        }

        /// 额外：HistoryNavigated Up 在空历史时不 panic
        #[test]
        fn test_reduce_history_navigated_up_empty_history() {
            let mut state = s();
            let effects = state.reduce(Action::HistoryNavigated(HistoryDir::Up));
            assert!(has_request_redraw(&effects));
        }

        /// 额外：InputCancelled 清空 buffer
        #[test]
        fn test_reduce_input_cancelled_clears_buffer() {
            let mut state = s();
            for ch in "draft".chars() {
                let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)));
            }
            assert_eq!(state.ui.input.text(), "draft");
            let effects = state.reduce(Action::InputCancelled);
            assert!(has_request_redraw(&effects));
            assert!(state.ui.input.is_empty());
        }

        #[test]
        fn ctrl_r_reverse_search_updates_state_and_snapshot_input() {
            use crate::chat::tui::ConversationLine;
            let mut state = s();
            state.ui.input.history = vec!["alpha".to_string(), "beta".to_string()];
            state.ui.conversation_lines.push(ConversationLine::Reasoning {
                content: "thinking".to_string(),
                char_count: 8,
                folded: true,
            });
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
            )));
            assert!(has_request_redraw(&effects));
            assert!(state.ui.input.is_reverse_search_active());
            assert_eq!(state.ui.input.text(), "beta");
            match state.ui.conversation_lines.last() {
                Some(ConversationLine::Reasoning { folded, .. }) => {
                    assert!(*folded, "Ctrl+R must not fold reasoning after P6b2");
                }
                other => panic!("expected Reasoning card, got {other:?}"),
            }
            let snap = state.build_ui_snapshot(1);
            assert!(snap.input.is_reverse_search_active());
            assert_eq!(snap.input.text(), "beta");
        }

        #[test]
        fn input_replaced_updates_snapshot_without_submit() {
            let mut state = s();
            state.ui.input.history = vec!["prior draft".to_string()];
            let _ = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
            )));
            assert!(
                state.ui.input.is_reverse_search_active(),
                "test setup: reverse-search must be active before external editor replacement"
            );
            let effects = state.reduce(Action::InputReplaced("edited draft".to_string()));
            assert!(has_request_redraw(&effects));
            assert_eq!(state.ui.input.text(), "edited draft");
            assert!(
                !state.ui.input.is_reverse_search_active(),
                "external editor replacement must clear stale reverse-search title"
            );
            assert_eq!(state.ui.turn_count, 0, "external editor replacement must not submit");
            let snap = state.build_ui_snapshot(2);
            assert_eq!(snap.input.text(), "edited draft");
            assert!(
                !snap.input.is_reverse_search_active(),
                "snapshot must not expose stale reverse-search after replacement"
            );
        }

        /// 额外：ReasoningFoldToggled 在无 reasoning 卡片时也返回 RequestRedraw
        #[test]
        fn test_reduce_reasoning_fold_toggled_no_panic_when_absent() {
            let mut state = s();
            let effects = state.reduce(Action::ReasoningFoldToggled);
            assert!(has_request_redraw(&effects));
        }

        /// BUG-01: Tab must expand the most recent Reasoning ("thinking") card,
        /// not only ToolResult cards. Previously Tab ignored Reasoning entirely,
        /// so thinking cards stayed folded ("▸") forever despite the
        /// "press Tab to expand" hint.
        #[test]
        fn test_tab_toggles_reasoning_card_fold() {
            use crate::chat::tui::ConversationLine;
            use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
            let mut state = s();
            state.ui.conversation_lines.push(ConversationLine::Reasoning {
                content: "deep thoughts".to_string(),
                char_count: 12,
                folded: true,
            });
            // Tab → expand.
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            assert!(has_request_redraw(&effects));
            match state.ui.conversation_lines.last() {
                Some(ConversationLine::Reasoning { folded, .. }) => {
                    assert!(!folded, "Tab must expand the reasoning card");
                }
                other => panic!("expected Reasoning card, got {other:?}"),
            }
            // Tab again → collapse.
            let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            match state.ui.conversation_lines.last() {
                Some(ConversationLine::Reasoning { folded, .. }) => {
                    assert!(folded, "second Tab must collapse the reasoning card");
                }
                other => panic!("expected Reasoning card, got {other:?}"),
            }
        }

        /// BUG-01: a fold toggle (KeyPressed Tab) must mark the reduce dirty so
        /// `build_ui_snapshot` rebuilds with the new folded state (the Pure-mode
        /// renderer reads the snapshot, not the live state). Guards against the
        /// stale `cached_lines_arc` regression.
        #[test]
        fn test_fold_toggle_marks_snapshot_dirty_and_rebuilds() {
            use crate::chat::tui::ConversationLine;
            use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
            let mut state = s();
            state.ui.conversation_lines.push(ConversationLine::Reasoning {
                content: "x".to_string(),
                char_count: 1,
                folded: true,
            });
            // Prime the snapshot cache.
            let _ = state.build_ui_snapshot(1);
            // Toggle via tracked reduce — must report dirty and clear the cache.
            let (_effects, dirty) =
                state.reduce_tracked(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            assert!(dirty, "fold toggle must mark snapshot dirty");
            let snap = state.build_ui_snapshot(2);
            match snap.conversation_lines.last() {
                Some(ConversationLine::Reasoning { folded, .. }) => {
                    assert!(!folded, "rebuilt snapshot must reflect the expanded card");
                }
                other => panic!("expected Reasoning card in snapshot, got {other:?}"),
            }
        }

        /// BUG-01 round-2: a fold toggle must bump `conversation_generation` so
        /// the snapshot/repaint path observes the new fold state.
        #[test]
        fn test_fold_toggle_bumps_conversation_generation() {
            use crate::chat::tui::ConversationLine;
            use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
            let mut state = s();
            state.ui.conversation_lines.push(ConversationLine::Reasoning {
                content: "thoughts".to_string(),
                char_count: 8,
                folded: true,
            });
            let gen_before = state.ui.conversation_generation;

            // Tab (foldable toggle) must bump the generation so scrollback re-emits.
            let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            assert_eq!(
                state.ui.conversation_generation,
                gen_before + 1,
                "Tab fold toggle must bump conversation_generation to force scrollback re-emit"
            );

            // Direct legacy fold action still bumps generation, but KeyPressed
            // Ctrl+R is reverse-search after P6b2 and must not own folding.
            let gen_after_tab = state.ui.conversation_generation;
            let _ = state.reduce(Action::ReasoningFoldToggled);
            assert_eq!(
                state.ui.conversation_generation,
                gen_after_tab + 1,
                "ReasoningFoldToggled must bump conversation_generation"
            );
        }

        /// BUG-01 round-2: a fold toggle with NO foldable card present must NOT
        /// bump the generation (avoids spurious full-conversation re-emits / a
        /// scrollback flood on stray Tab presses in a fresh session).
        #[test]
        fn test_fold_toggle_no_card_does_not_bump_generation() {
            use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
            let mut state = s();
            let gen_before = state.ui.conversation_generation;
            let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
            assert_eq!(
                state.ui.conversation_generation, gen_before,
                "Tab with no foldable card must not bump generation"
            );
        }

        // ─── Step 2 集成测试（P1-1 PTY 覆盖空洞补全）──────────────────────────
        //
        // 以下测试通过直接构造 ChatState + 调用 reduce 序列，模拟完整用户交互流程，
        // 覆盖 run_tui_unified_loop 中 reducer 路径（PRX_CHAT_REDUX=1/both 灰度范围）。
        // 使用 reduce_with_now 注入确定时间，避开 SystemTime 依赖。

        /// P1-1-a: 完整输入提交流程 — paste "hello" → Enter → 期望含 LogTrace + RequestRedraw
        #[test]
        fn test_redux_full_input_to_submit_flow() {
            let mut state = s();
            // 粘贴 "hello"
            let effects = state.reduce(Action::PasteReceived("hello".to_string()));
            assert!(has_request_redraw(&effects), "paste 应触发 RequestRedraw");
            assert_eq!(state.ui.input.text(), "hello");
            // 按 Enter 提交
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
            assert!(has_log_trace(&effects), "Enter 应触发 LogTrace");
            assert!(has_request_redraw(&effects), "Enter 应触发 RequestRedraw");
            assert_eq!(state.ui.turn_count, 1, "turn_count 应递增至 1");
            assert_eq!(
                state.ui.last_submitted.as_deref(),
                Some("hello"),
                "last_submitted 应记录 'hello'"
            );
            assert!(state.ui.input.is_empty(), "提交后 input buffer 应清空");
        }

        /// P1-1-b: 双 Ctrl+C 在 500ms 内 → Quit
        #[test]
        fn test_redux_double_ctrl_c_within_500ms_quits() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let effects = state.reduce_with_now(Action::KeyPressed(key.clone()), 100);
            assert!(!has_quit(&effects), "第一次 Ctrl+C 不应 Quit");
            let effects = state.reduce_with_now(Action::KeyPressed(key), 300);
            assert!(has_quit(&effects), "500ms 内双击 Ctrl+C 应产生 Quit effect");
        }

        /// P1-1-c: Ctrl+C 间隔超过 500ms 不退出
        #[test]
        fn test_redux_ctrl_c_then_ctrl_c_after_500ms_does_not_quit() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
            let effects = state.reduce_with_now(Action::KeyPressed(key.clone()), 100);
            assert!(!has_quit(&effects));
            // 700ms 后再按 — 超过 500ms 窗口
            let effects = state.reduce_with_now(Action::KeyPressed(key), 700);
            assert!(!has_quit(&effects), "超过 500ms 的双击不应 Quit");
            assert_eq!(state.ui.last_ctrlc_ms, 700);
        }

        /// P1-1-d: Ctrl+D 空 buffer → Quit
        #[test]
        fn test_redux_ctrl_d_empty_buffer_quits() {
            let mut state = s();
            assert!(state.ui.input.is_empty(), "前提：buffer 为空");
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )));
            assert!(has_quit(&effects), "空 buffer Ctrl+D 应 Quit");
        }

        /// P1-1-e: Ctrl+D 非空 buffer → 不退出（forward-delete）
        #[test]
        fn test_redux_ctrl_d_non_empty_does_not_quit() {
            let mut state = s();
            // 输入 "xyz" 然后 Home 移到行首，使光标前有内容
            let _ = state.reduce(Action::PasteReceived("xyz".to_string()));
            let _ = state.reduce(Action::KeyPressed(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)));
            assert_eq!(state.ui.input.text(), "xyz");
            let effects = state.reduce(Action::KeyPressed(KeyEvent::new(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
            )));
            assert!(!has_quit(&effects), "非空 buffer Ctrl+D 不应 Quit");
        }

        /// P1-1-f: HistoryNavigated Up/Down — 空历史时不 panic，返回 RequestRedraw
        #[test]
        fn test_redux_history_navigation_up_down() {
            let mut state = s();
            let up = state.reduce(Action::HistoryNavigated(HistoryDir::Up));
            assert!(has_request_redraw(&up), "Up 应返回 RequestRedraw");
            let down = state.reduce(Action::HistoryNavigated(HistoryDir::Down));
            assert!(has_request_redraw(&down), "Down 应返回 RequestRedraw");
        }

        /// P1-1-g: PasteReceived → input buffer 含文本 + RequestRedraw
        #[test]
        fn test_redux_paste_into_input() {
            let mut state = s();
            let effects = state.reduce(Action::PasteReceived("pasted content".to_string()));
            assert!(has_request_redraw(&effects), "粘贴应触发 RequestRedraw");
            assert_eq!(state.ui.input.text(), "pasted content");
        }

        /// P1-1-h: TerminalResized → RequestRedraw
        #[test]
        fn test_redux_terminal_resize_returns_redraw() {
            let mut state = s();
            let effects = state.reduce(Action::TerminalResized { w: 80, h: 24 });
            assert!(has_request_redraw(&effects), "resize 应触发 RequestRedraw");
        }

        // ─── Step 3 单元测试（流式 + 工具路径） ────────────────────────────────
        //
        // 覆盖目标：
        //   - 5 个流式 Action (TurnStarted/StreamChunkReceived/StreamCompleted/
        //     StreamFailed/StreamCancelled) + 3 个工具 Action
        //     (ToolStarted/ToolFinished/ToolProgress)
        //   - P3-5 版本号防护下沉至 reducer 后的所有 stale-drop 路径
        //   - 与 finalize_draft 重试路径的幂等性边界
        //
        // 这些测试是 P3-5 版本号机制完整下沉到 reducer 的核心证据。

        /// Step3-1: TurnStarted 初始化 stream.draft + active_cancel + generating
        #[test]
        fn test_redux_turn_started_sets_stream_state() {
            let mut state = s();
            assert!(state.stream.primary_streaming_draft().is_none());
            assert!(state.control.active_cancel.is_none());
            assert!(!state.control.generating);
            let effects = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(state.stream.primary_streaming_draft().is_some());
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.draft_id.clone()),
                Some("d1".to_string())
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(0));
            assert!(state.control.active_cancel.is_some());
            assert!(state.control.generating);
            assert!(has_request_redraw(&effects));
            assert!(has_log_trace(&effects));
        }

        /// Step3-2: 正常 chunk → 累积 + version 更新 + RequestRedraw
        #[test]
        fn test_redux_stream_chunk_received_valid_appends() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "hello".to_string(),
                version: 1,
            });
            assert!(has_request_redraw(&effects));
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("hello".to_string())
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(1));

            // 第二个有效 chunk → 累积
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: " world".to_string(),
                version: 2,
            });
            assert!(has_request_redraw(&effects));
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("hello world".to_string())
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(2));
        }

        /// Step3-3: stale version（version=1 在 version=2 之后到达）→ 丢弃
        #[test]
        fn test_redux_stream_chunk_received_stale_version_dropped() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            // 先收到 version=2
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "AB".to_string(),
                version: 2,
            });
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("AB".to_string())
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(2));
            // 后来才到 version=1 → 丢弃
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "STALE".to_string(),
                version: 1,
            });
            assert!(effects.is_empty(), "stale version 应返回空 effects");
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("AB".to_string()),
                "accumulated 应保持不变"
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(2));

            // 重复 version=2 → 也丢弃（strict-monotonic）
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "DUP".to_string(),
                version: 2,
            });
            assert!(effects.is_empty(), "重复 version 应丢弃");
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("AB".to_string())
            );
        }

        /// Step3-4: 跨 turn draft_id 不匹配 → 丢弃
        #[test]
        fn test_redux_stream_chunk_received_wrong_draft_id_dropped() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "ok".to_string(),
                version: 1,
            });
            // 错误 draft_id → 丢弃
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d2".to_string(),
                delta: "STALE".to_string(),
                version: 99,
            });
            assert!(effects.is_empty(), "draft_id 不匹配应返回空");
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("ok".to_string())
            );
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(1));
        }

        /// Step3-5: finalize 后再到达的 chunk → 丢弃
        #[test]
        fn test_redux_stream_chunk_received_after_finalize_dropped() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "complete".to_string(),
                version: 1,
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "complete".to_string(),
                reasoning: String::new(),
            });
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "finalize 后 draft 应清空"
            );
            // 此后 chunk 视为 stale
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "LATE".to_string(),
                version: 2,
            });
            assert!(effects.is_empty(), "finalize 后 chunk 应丢弃");
            assert!(state.stream.primary_streaming_draft().is_none());
        }

        /// Step3-6: StreamCompleted 清除 draft + push assistant + NotifyHook
        #[test]
        fn test_redux_stream_completed_clears_draft_and_pushes_assistant() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(state.control.generating);
            let prev_lines = state.ui.conversation_lines.len();
            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "final answer".to_string(),
                reasoning: String::new(),
            });
            assert!(state.stream.primary_streaming_draft().is_none(), "draft 应清空");
            assert!(state.control.active_cancel.is_none());
            assert!(!state.control.generating);
            assert_eq!(
                state.ui.conversation_lines.len(),
                prev_lines + 1,
                "应 push 1 个 Assistant 行"
            );
            // 验证最后一行是 Assistant("final answer")
            if let Some(crate::chat::tui::ConversationLine::Assistant { content }) = state.ui.conversation_lines.last()
            {
                assert_eq!(content, "final answer");
            } else {
                panic!("最后一行应是 ConversationLine::Assistant");
            }
            assert!(has_request_redraw(&effects));
            assert!(
                effects.iter().any(|e| matches!(e, Effect::NotifyHook { .. })),
                "应包含 NotifyHook(TurnComplete)"
            );
        }

        /// Step3-6b: StreamCompleted with reasoning → 同时 push Reasoning 卡片
        #[test]
        fn test_redux_stream_completed_with_reasoning_pushes_card() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "ans".to_string(),
                reasoning: "thinking step".to_string(),
            });
            // 应有 Assistant + Reasoning 共 2 行
            assert_eq!(state.ui.conversation_lines.len(), 2);
            assert!(
                matches!(
                    state.ui.conversation_lines.last(),
                    Some(crate::chat::tui::ConversationLine::Reasoning { .. })
                ),
                "最后一行应是 Reasoning"
            );
        }

        /// Step3-7: StreamFailed 清除 draft + WARN LogTrace
        #[test]
        fn test_redux_stream_failed_clears_draft() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "partial".to_string(),
                version: 1,
            });
            let effects = state.reduce(Action::StreamFailed {
                draft_id: "d1".to_string(),
                err: "network".to_string(),
                retryable: true,
            });
            assert!(state.stream.primary_streaming_draft().is_none());
            assert!(!state.control.generating);
            assert!(has_request_redraw(&effects));
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::LogTrace { level, .. } if *level == tracing::Level::WARN)),
                "应包含 WARN LogTrace"
            );
        }

        /// Step3-8: StreamCancelled 清除 draft + 不 push 任何消息
        #[test]
        fn test_redux_stream_cancelled_clears_draft() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "partial".to_string(),
                version: 1,
            });
            let lines_before = state.ui.conversation_lines.len();
            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "d1".to_string(),
            });
            assert!(state.stream.primary_streaming_draft().is_none());
            assert!(!state.control.generating);
            assert!(state.control.active_cancel.is_none());
            assert!(has_request_redraw(&effects));
            assert_eq!(
                state.ui.conversation_lines.len(),
                lines_before,
                "cancel 不应 push 任何 conversation line"
            );
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn stream_cancelled_finalizes_pending_running_tool_cards() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-tool-cancel".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"command":"sleep 10"}"#.to_string(),
            });

            let _ = state.reduce(Action::StreamCancelled {
                draft_id: "d-tool-cancel".to_string(),
            });

            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 0);
            assert!(
                !crate::chat::tui::execution_activity_active_for_view(&state.build_ui_snapshot(1)),
                "cancelled turn must not leave a running tool card driving the status bar"
            );
            assert!(
                state.ui.conversation_lines.iter().any(|line| matches!(
                    line,
                    crate::chat::tui::ConversationLine::ToolResult {
                        tool_name,
                        status: crate::chat::tui::ToolStatus::Error,
                        ..
                    } if tool_name == "shell"
                )),
                "pending shell tool card should be finalized as an error/cancelled card"
            );
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn cancel_requested_finalizes_pending_running_tool_cards() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-tool-cancel-request".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"command":"sleep 10"}"#.to_string(),
            });

            let _ = state.reduce(Action::CancelRequested);

            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 0);
            assert!(
                !crate::chat::tui::execution_activity_active_for_view(&state.build_ui_snapshot(1)),
                "cancel request must not leave a running tool card driving the status bar"
            );
            assert!(
                state.ui.conversation_lines.iter().any(|line| matches!(
                    line,
                    crate::chat::tui::ConversationLine::ToolResult {
                        tool_name,
                        status: crate::chat::tui::ToolStatus::Error,
                        result: Some(result),
                        ..
                    } if tool_name == "shell" && result.contains("cancel request")
                )),
                "cancel request should finalize the shell tool card"
            );
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn stream_completed_removes_unfinished_pending_tool_cards() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-tool-complete".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"command":"sleep 10"}"#.to_string(),
            });

            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d-tool-complete".to_string(),
                final_text: "done".to_string(),
                reasoning: String::new(),
            });

            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 0);
            assert!(
                !state
                    .ui
                    .conversation_lines
                    .iter()
                    .any(|line| matches!(line, crate::chat::tui::ConversationLine::ToolResult { .. })),
                "completed turn should remove unfinished placeholder tool cards"
            );
            assert!(
                !crate::chat::tui::execution_activity_active_for_view(&state.build_ui_snapshot(1)),
                "completed turn must not leave placeholder tool cards driving the status bar"
            );
        }

        #[test]
        fn stream_failed_removes_trailing_answerless_user_turn() {
            let mut state = s();
            let _ = state.reduce(Action::RecordUserTurn("failed question".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-failed".to_string(),
                cancel: CancellationToken::new(),
            });

            let effects = state.reduce(Action::StreamFailed {
                draft_id: "d-failed".to_string(),
                err: "provider failed".to_string(),
                retryable: false,
            });

            assert!(has_request_redraw(&effects));
            assert!(
                state.session.turns.is_empty(),
                "GP-9: failed turn must not leave an answerless user turn"
            );
            assert!(
                state
                    .session
                    .history
                    .iter()
                    .all(|message| message.content != "failed question"),
                "failed turn must also be removed from reducer history"
            );
            assert!(
                state.session.title.is_empty(),
                "first failed prompt must not become the session title"
            );
        }

        #[test]
        fn stream_cancelled_removes_trailing_answerless_user_turn() {
            let mut state = s();
            let _ = state.reduce(Action::RecordUserTurn("cancelled question".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-cancelled".to_string(),
                cancel: CancellationToken::new(),
            });

            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "d-cancelled".to_string(),
            });

            assert!(has_request_redraw(&effects));
            assert!(
                state.session.turns.is_empty(),
                "GP-9: cancelled turn must not leave an answerless user turn"
            );
            assert!(
                state
                    .session
                    .history
                    .iter()
                    .all(|message| message.content != "cancelled question"),
                "cancelled turn must also be removed from reducer history"
            );
        }

        #[test]
        fn failed_turn_orphan_is_absent_from_background_and_later_success_snapshots() {
            let mut state = s();
            let _ = state.reduce(Action::RecordUserTurn("orphan candidate".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-failed".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamFailed {
                draft_id: "d-failed".to_string(),
                err: "provider failed".to_string(),
                retryable: false,
            });

            let background_effects = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-after-failure", "completed"),
            });
            let background_snapshot = background_effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::SaveSession(snapshot) => Some(snapshot),
                    _ => None,
                })
                .expect("BackgroundSessionRecorded must emit SaveSession");
            assert!(
                background_snapshot
                    .turns
                    .iter()
                    .all(|turn| turn.content != "orphan candidate"),
                "GP-9: background save snapshot must not persist the failed user turn"
            );

            let _ = state.reduce(Action::RecordUserTurn("real question".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-ok".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "real answer".to_string(),
            });
            let completion_effects = state.reduce(Action::StreamCompleted {
                draft_id: "d-ok".to_string(),
                final_text: "real answer".to_string(),
                reasoning: String::new(),
            });
            let completion_snapshot = completion_effects
                .iter()
                .find_map(|effect| match effect {
                    Effect::SaveSession(snapshot) => Some(snapshot),
                    _ => None,
                })
                .expect("StreamCompleted must emit SaveSession");
            assert_eq!(
                completion_snapshot.turns.len(),
                2,
                "only the successful exchange is persisted"
            );
            assert_eq!(
                completion_snapshot.turns.first().map(|turn| turn.content.as_str()),
                Some("real question")
            );
            assert_eq!(
                completion_snapshot.turns.get(1).map(|turn| turn.content.as_str()),
                Some("real answer")
            );
        }

        /// Step3-8b: 不匹配 draft_id 的 StreamCancelled / StreamFailed / StreamCompleted → no-op
        #[test]
        fn test_redux_stream_terminal_actions_wrong_id_noop() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            // 用错误 id 触发三个终止 action — 全部应 no-op
            let e1 = state.reduce(Action::StreamCancelled {
                draft_id: "wrong".to_string(),
            });
            let e2 = state.reduce(Action::StreamFailed {
                draft_id: "wrong".to_string(),
                err: "x".to_string(),
                retryable: false,
            });
            let e3 = state.reduce(Action::StreamCompleted {
                draft_id: "wrong".to_string(),
                final_text: "x".to_string(),
                reasoning: String::new(),
            });
            assert!(e1.is_empty() && e2.is_empty() && e3.is_empty());
            assert!(state.stream.primary_streaming_draft().is_some(), "原 draft 应保留");
            assert!(state.control.generating, "generating 标志应保留");
        }

        /// Step3-9: ToolStarted → push Running ToolResult + 索引入队
        #[test]
        fn test_redux_tool_started_pushes_card() {
            use crate::chat::tui::{ConversationLine, ToolStatus};
            let mut state = s();
            let effects = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            });
            assert!(has_request_redraw(&effects));
            assert_eq!(state.ui.conversation_lines.len(), 1);
            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Primary), 1);
            if let Some(ConversationLine::ToolResult { tool_name, status, .. }) = state.ui.conversation_lines.last() {
                assert_eq!(tool_name, "shell");
                assert_eq!(*status, ToolStatus::Running);
            } else {
                panic!("最后一行应是 Running ToolResult");
            }
        }

        /// Step3-10: ToolFinished → Running → Done + 从 pending 移除
        #[test]
        fn test_redux_tool_finished_updates_card() {
            use crate::chat::tui::{ConversationLine, ToolStatus};
            let mut state = s();
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            });
            let effects = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                success: true,
                duration_ms: 42,
                result: Some("ok".to_string()),
            });
            assert!(has_request_redraw(&effects));
            assert_eq!(
                state.control.pending_tool_card_count(ToolTaskKey::Primary),
                0,
                "pending 应被清空"
            );
            if let Some(ConversationLine::ToolResult {
                status,
                elapsed_ms,
                result,
                ..
            }) = state.ui.conversation_lines.last()
            {
                assert_eq!(*status, ToolStatus::Done);
                assert_eq!(*elapsed_ms, Some(42));
                assert_eq!(result.as_deref(), Some("ok"));
            } else {
                panic!("最后一行应是 ToolResult");
            }
        }

        /// Step3-10b: ToolFinished success=false → Error
        #[test]
        fn test_redux_tool_finished_failed_marks_error() {
            use crate::chat::tui::{ConversationLine, ToolStatus};
            let mut state = s();
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{}"#.to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                success: false,
                duration_ms: 10,
                result: Some("err".to_string()),
            });
            if let Some(ConversationLine::ToolResult { status, .. }) = state.ui.conversation_lines.last() {
                assert_eq!(*status, ToolStatus::Error);
            } else {
                panic!("最后一行应是 ToolResult");
            }
        }

        /// Step3-11: ToolProgress returns redraw/log and bumps UI generation.
        #[test]
        fn test_redux_tool_progress_returns_log_redraw_and_visible_generation() {
            let mut state = s();
            let before = state.ui.conversation_generation;
            let effects = state.reduce(Action::ToolProgress { iteration: 3, max: 10 });
            assert!(has_request_redraw(&effects));
            assert!(has_log_trace(&effects));
            assert!(state.ui.conversation_lines.is_empty());
            assert_eq!(state.ui.conversation_generation, before + 1);
        }

        /// Step3-12: finalize 路径的幂等性 — 即便 StreamCompleted 被错误地重复
        /// 触发，第二次 reduce 应是 no-op（draft_id 已不存在）
        #[test]
        fn test_redux_finalize_retry_after_stream_completed_idempotent() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "ans".to_string(),
                reasoning: String::new(),
            });
            let lines_after_first = state.ui.conversation_lines.len();
            // 再次 finalize 相同 draft_id — 此时 draft 已清空 → no-op
            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "d1".to_string(),
                final_text: "ans-dup".to_string(),
                reasoning: String::new(),
            });
            assert!(effects.is_empty(), "重复 finalize 应是 no-op");
            assert_eq!(
                state.ui.conversation_lines.len(),
                lines_after_first,
                "不应 push 重复 assistant 行（幂等）"
            );
        }

        /// Step3-13: StreamFailed 后重试新一轮 Turn — 版本号从 0 重新开始
        #[test]
        fn test_redux_finalize_retry_after_stream_failed() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "partial".to_string(),
                version: 5,
            });
            let _ = state.reduce(Action::StreamFailed {
                draft_id: "d1".to_string(),
                err: "boom".to_string(),
                retryable: true,
            });
            assert!(state.stream.primary_streaming_draft().is_none());
            // 重试：开一个新 turn (相同 draft_id 也 OK，draft.version 从 0 起)
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            assert_eq!(state.stream.primary_streaming_draft().map(|d| d.version), Some(0));
            // version=1 应被接受（不被前一轮的 5 影响 — 因为 draft 已重建）
            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "d1".to_string(),
                delta: "retry".to_string(),
                version: 1,
            });
            assert!(has_request_redraw(&effects), "新 turn 的 v=1 应被接受");
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.accumulated.clone()),
                Some("retry".to_string())
            );
        }

        #[test]
        fn esc_key_during_generation_cancels_active_turn() {
            use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-esc".to_string(),
                cancel: CancellationToken::new(),
            });
            state.ui.input.set_text("local draft");

            let effects = state.reduce_with_now(
                Action::KeyPressed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                1_000,
            );

            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::CancelDraft(id) if id == "draft-esc")),
                "Esc while generating must cancel the active draft: {effects:?}"
            );
            assert!(!state.control.generating);
            assert_eq!(state.ui.input.text(), "local draft");
        }

        /// P1-2: Both 模式下连续 10 个正常 Action — 无语义差异（diff_count 基线验证）.
        ///
        /// 此测试验证 reducer 自身行为稳定；实际 diff_count 跨进程不可查，
        /// 因此通过直接检查 reduce 输出的语义一致性（同一 Action 序列下 effects 稳定）
        /// 作为等价验证。
        #[test]
        fn test_redux_both_mode_diff_count_zero() {
            let mut state1 = s();
            let mut state2 = s();
            // 对两个独立 state 跑相同 Action 序列，期望 effects 语义类别完全一致
            let actions: Vec<Action> = vec![
                Action::PasteReceived("hello".to_string()),
                Action::TerminalResized { w: 120, h: 40 },
                Action::RedrawRequested,
                Action::ToolCardFoldToggled,
                Action::ReasoningFoldToggled,
                Action::HistoryNavigated(HistoryDir::Up),
                Action::HistoryNavigated(HistoryDir::Down),
                Action::InputCancelled,
                Action::InputSubmitted("test input".to_string()),
                Action::RedrawRequested,
            ];
            for action in actions {
                let e1 = state1.reduce(action.clone());
                let e2 = state2.reduce(action);
                // 两次独立执行相同 action，effects 类别数量应一致
                assert_eq!(
                    e1.len(),
                    e2.len(),
                    "同 Action 在两个独立 state 上应产生相同数量的 effects"
                );
            }
        }

        // ─── Step 4 单元测试（退出 + 会话路径） ────────────────────────────────

        /// Step4-1: generating=false 时 CancelRequested → no-op（vec![]）
        #[test]
        fn test_redux_cancel_requested_no_active_turn_noop() {
            let mut state = s();
            assert!(!state.control.generating, "前提：未在生成中");
            let effects = state.reduce(Action::CancelRequested);
            assert!(effects.is_empty(), "非生成中 CancelRequested 应返回 vec![]");
        }

        /// Step4-2: generating=true, 有 draft 时 CancelRequested → 清 draft + CancelDraft effect
        #[test]
        fn test_redux_cancel_requested_with_active_turn_clears_state() {
            let mut state = s();
            // 开始一轮流式
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(state.control.generating);
            assert!(state.stream.primary_streaming_draft().is_some());

            let effects = state.reduce(Action::CancelRequested);

            // 状态应已清除
            assert!(!state.control.generating, "generating 应清为 false");
            assert!(state.stream.primary_streaming_draft().is_none(), "draft 应清空");
            assert!(state.control.active_cancel.is_none(), "active_cancel 应清空");
            // effects 应含 CancelDraft + LogTrace + RequestRedraw
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::CancelDraft(id) if id == "d1")),
                "应包含 CancelDraft(d1)"
            );
            assert!(has_log_trace(&effects), "应包含 LogTrace");
            assert!(has_request_redraw(&effects), "应包含 RequestRedraw");
        }

        /// Step4-3: ShutdownRequested (idle) → vec![Quit]
        #[test]
        fn test_redux_shutdown_requested_returns_quit() {
            let mut state = s();
            let effects = state.reduce(Action::ShutdownRequested);
            assert!(has_quit(&effects), "ShutdownRequested 应返回 Quit effect");
            // 空闲时无 CancelDraft
            assert!(
                !effects.iter().any(|e| matches!(e, Effect::CancelDraft(_))),
                "空闲 shutdown 不应含 CancelDraft"
            );
        }

        /// Step4-4: 流式中 ShutdownRequested → Quit + CancelDraft
        #[test]
        fn test_redux_shutdown_during_streaming_cancels_draft() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d2".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(state.control.generating);

            let effects = state.reduce(Action::ShutdownRequested);

            assert!(!state.control.generating, "generating 应清除");
            assert!(state.stream.primary_streaming_draft().is_none(), "draft 应清空");
            // S2-B Step 2: effect 顺序变为 [CancelToken, CancelDraft, Quit].
            // CancelToken 在前（真取消底层 turn），CancelDraft 紧随（同步 channel UI），
            // Quit 在最后（外壳调 shutdown.cancel()）。
            assert!(effects.len() >= 3, "流式 ShutdownRequested 应至少 3 个 effect");
            assert!(
                matches!(effects.first(), Some(Effect::CancelToken(_))),
                "effects[0] 应为 CancelToken，实际: {:?}",
                effects.first()
            );
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::CancelDraft(id) if id == "d2")),
                "effects 必须含 CancelDraft(d2)"
            );
            assert!(
                matches!(effects.last(), Some(Effect::Quit)),
                "effects.last() 应为 Quit，实际: {:?}",
                effects.last()
            );
        }

        /// Step4-5: SessionLoaded 替换 session 全部字段
        #[test]
        fn test_redux_session_loaded_replaces_session_state() {
            use crate::chat::session::ChatSession;
            let mut state = s();
            // 给 history 加点东西，确认会被替换
            state.session.history.push(crate::providers::ChatMessage::user("old"));

            let mut loaded = ChatSession::new("prov2", "model2");
            loaded.id = "sess-abc".to_string();
            loaded.title = "My Session".to_string();
            loaded.add_user_turn("hello");
            loaded.add_assistant_turn("hi", vec![]);

            let effects = state.reduce(Action::SessionLoaded(loaded));

            assert_eq!(state.session.id, "sess-abc");
            assert_eq!(state.session.title, "My Session");
            assert_eq!(&*state.session.provider, "prov2");
            assert_eq!(&*state.session.model, "model2");
            assert_eq!(state.session.turns.len(), 2, "2 个 turn");
            // history 从 turns 重建：user + assistant
            assert_eq!(
                state.session.history.len(),
                2,
                "history 应从 turns 重建(user+assistant)"
            );
            assert_eq!(
                state.ui.conversation_lines.len(),
                2,
                "UI conversation_lines 应从恢复的 turns 重建"
            );
            assert!(has_request_redraw(&effects), "应含 RequestRedraw");
            assert!(has_log_trace(&effects), "应含 LogTrace");
        }

        fn bg_summary(id: &str, status: &str) -> crate::chat::sessions::PersistedSessionSummary {
            crate::chat::sessions::PersistedSessionSummary {
                id: id.to_string(),
                seq: 1,
                kind: "agent".to_string(),
                origin: "user".to_string(),
                status: status.to_string(),
                title: "task".to_string(),
                summary: String::new(),
                token_usage_records: Vec::new(),
                created_at: chrono::Utc::now(),
            }
        }

        /// v4: BackgroundSessionRecorded upserts into session.background_sessions
        /// (dedup by id) and emits SaveSession so the summary is persisted.
        #[test]
        fn test_redux_background_session_recorded_upserts() {
            let mut state = s();
            let e1 = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-1", "running"),
            });
            // P0 (v4 review): a record that changed state must emit SaveSession
            // (the only durable write path). It must NOT redraw — a background
            // summary write is invisible to the live conversation surface.
            assert_eq!(e1.len(), 1, "exactly one effect: SaveSession");
            assert!(
                matches!(e1.first(), Some(Effect::SaveSession(_))),
                "changed record must emit SaveSession, got {e1:?}"
            );
            assert_eq!(state.session.background_sessions.len(), 1);

            // Same id again with a terminal status replaces, does not duplicate,
            // and still emits a fresh SaveSession (state changed). Reuse the same
            // value for the no-op check below (bg_summary stamps a fresh
            // created_at per call, which would otherwise count as a change).
            let completed = bg_summary("run-1", "completed");
            let e2 = state.reduce(Action::BackgroundSessionRecorded {
                summary: completed.clone(),
            });
            assert!(matches!(e2.first(), Some(Effect::SaveSession(_))));
            assert_eq!(state.session.background_sessions.len(), 1);
            assert_eq!(
                state.session.background_sessions.first().map(|s| s.status.as_str()),
                Some("completed")
            );

            // An identical re-record is a no-op: no state change, no effect, no
            // save storm.
            let e_dup = state.reduce(Action::BackgroundSessionRecorded { summary: completed });
            assert!(
                e_dup.is_empty(),
                "unchanged re-record must short-circuit with no SaveSession (no save storm)"
            );

            // A different id appends and saves.
            let e3 = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-2", "failed"),
            });
            assert!(matches!(e3.first(), Some(Effect::SaveSession(_))));
            assert_eq!(state.session.background_sessions.len(), 2);
        }

        /// B1 (P0, v4 review): dispatching BackgroundSessionRecorded must emit a
        /// SaveSession whose snapshot ALREADY contains the just-recorded summary.
        /// This is the regression guard: previously the reducer returned no
        /// effect, so the terminal-summary never reached the memory backend
        /// (legacy exit-save is disabled under terminal-tui), breaking reload
        /// recap. Round-trip: capture the SaveSession snapshot → reload it into a
        /// fresh state → the background summary is present.
        #[test]
        fn test_redux_background_session_recorded_emits_savesession_with_summary() {
            let mut state = s();
            let effects = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-42", "completed"),
            });
            let snapshot = effects
                .iter()
                .find_map(|e| match e {
                    Effect::SaveSession(session) => Some(session.clone()),
                    _ => None,
                })
                .expect("BackgroundSessionRecorded must emit Effect::SaveSession");
            // The emitted snapshot must already carry the recorded summary —
            // proving the save happens AFTER the upsert (no race where the
            // snapshot predates the state mutation).
            assert_eq!(
                snapshot.background_sessions.len(),
                1,
                "SaveSession snapshot must contain the just-recorded child session"
            );
            let recorded = snapshot
                .background_sessions
                .first()
                .expect("snapshot child session present");
            assert_eq!(recorded.id, "run-42");
            assert_eq!(recorded.status, "completed");

            // Round-trip: reloading that snapshot into a fresh state restores it,
            // confirming the persisted blob is sufficient for reload recap.
            let mut reloaded = s();
            let _ = reloaded.reduce(Action::SessionLoaded(snapshot));
            assert_eq!(reloaded.session.background_sessions.len(), 1);
            assert_eq!(
                reloaded.session.background_sessions.first().map(|s| s.id.as_str()),
                Some("run-42")
            );
        }

        /// v4: a recorded child session must survive a save→load round trip
        /// through the reducer (snapshot persists it, SessionLoaded restores it),
        /// and a still-running session is never restored as a live one.
        #[test]
        fn test_redux_background_sessions_survive_snapshot_and_reload() {
            let mut state = s();
            let _ = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-1", "completed"),
            });
            // An interrupted entry stands in for "was running at last exit".
            let _ = state.reduce(Action::BackgroundSessionRecorded {
                summary: bg_summary("run-2", crate::chat::sessions::model::STATUS_INTERRUPTED),
            });

            // The snapshot the SaveSession effect would persist must carry them.
            let snapshot = state.build_session_snapshot();
            assert_eq!(snapshot.background_sessions.len(), 2);

            // Reloading that snapshot into a fresh state restores the summaries.
            let mut fresh = s();
            let _ = fresh.reduce(Action::SessionLoaded(snapshot));
            assert_eq!(fresh.session.background_sessions.len(), 2);
            // None of the restored entries is a live status — reload never
            // resurrects a running process.
            for bg in &fresh.session.background_sessions {
                assert_ne!(bg.status, "running");
                assert_ne!(bg.status, "needs-input");
            }
            let statuses: Vec<&str> = fresh
                .session
                .background_sessions
                .iter()
                .map(|s| s.status.as_str())
                .collect();
            assert!(statuses.contains(&"completed"));
            assert!(statuses.contains(&crate::chat::sessions::model::STATUS_INTERRUPTED));
        }

        /// Step4-5b: SessionLoaded 含 system prompt — history 中保留 user/assistant，
        /// system turn 不进 LLM history（turns 里 role=system 不过滤进 history）
        #[test]
        fn test_redux_session_loaded_only_user_assistant_in_history() {
            use crate::chat::session::{ChatSession, ChatTurn};
            let mut state = s();
            let mut loaded = ChatSession::new("p", "m");
            loaded.turns.push(ChatTurn {
                role: "system".to_string(),
                content: "You are helpful".to_string(),
                timestamp: chrono::Utc::now(),
                tool_calls: vec![],
            });
            loaded.add_user_turn("q");
            let _ = state.reduce(Action::SessionLoaded(loaded));
            // history 只含 user，不含 system（系统在 SessionLoaded 路径不自动加入 history）
            assert_eq!(
                state.session.history.len(),
                1,
                "仅 user turn 进 history（role=system 过滤掉）"
            );
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("user"),
                "history[0] 应为 user role"
            );
        }

        /// Step4-6: SessionSaved 更新 session.id
        #[test]
        fn test_redux_session_saved_updates_id() {
            let mut state = s();
            state.session.id = String::new(); // 模拟还未有 id
            let effects = state.reduce(Action::SessionSaved {
                id: "new-id-123".to_string(),
            });
            assert_eq!(state.session.id, "new-id-123");
            assert!(has_log_trace(&effects), "应含 LogTrace");
        }

        /// Step4-6b: SessionSaved 相同 id — 不变（幂等）
        #[test]
        fn test_redux_session_saved_same_id_idempotent() {
            let mut state = s();
            state.session.id = "already-set".to_string();
            let effects = state.reduce(Action::SessionSaved {
                id: "already-set".to_string(),
            });
            assert_eq!(state.session.id, "already-set");
            assert!(has_log_trace(&effects));
        }

        /// Step4-7: SessionSwitched → SaveSession + LogTrace + RequestRedraw
        #[test]
        fn test_redux_session_switched_saves_current_then_logs() {
            let mut state = s();
            state.session.id = "cur-session".to_string();
            state.session.title = "Current".to_string();
            let effects = state.reduce(Action::SessionSwitched {
                id: "new-session".to_string(),
            });
            assert!(
                effects
                    .iter()
                    .any(|e| matches!(e, Effect::SaveSession(sess) if sess.id == "cur-session")),
                "应先 SaveSession 当前 session"
            );
            assert!(has_log_trace(&effects), "应含 LogTrace");
            assert!(has_request_redraw(&effects), "应含 RequestRedraw");
        }

        /// P2-C: SessionSwitched effects[0] 精确为 SaveSession（两步异步流程前置保存）
        #[test]
        fn test_redux_session_switched_emits_save_first() {
            let mut state = s();
            state.session.id = "session-x".to_string();
            let effects = state.reduce(Action::SessionSwitched {
                id: "session-y".to_string(),
            });
            assert!(!effects.is_empty(), "SessionSwitched 应至少有 1 个 effect");
            assert!(
                matches!(effects.first(), Some(Effect::SaveSession(sess)) if sess.id == "session-x"),
                "effects[0] 必须是 SaveSession(current)，实际: {:?}",
                effects.first()
            );
        }

        /// Step4-8: RecordUserTurn → session.turns + history 增长，updated_at 更新，首条 user 自动 set_title
        #[test]
        fn test_redux_record_user_turn_grows_history() {
            let mut state = s();
            assert_eq!(state.session.turns.len(), 0);
            assert_eq!(state.session.history.len(), 0);
            assert!(state.session.title.is_empty(), "初始 title 为空");
            let effects = state.reduce(Action::RecordUserTurn("what is Rust?".to_string()));
            assert_eq!(state.session.turns.len(), 1, "turns 增长");
            assert_eq!(state.session.history.len(), 1, "history 增长");
            assert_eq!(
                state.session.turns.first().map(|t| t.role.as_str()),
                Some("user"),
                "turns[0] role"
            );
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("user"),
                "history[0] role"
            );
            assert_eq!(
                state.session.history.first().map(|m| m.content.as_str()),
                Some("what is Rust?"),
                "history[0] content"
            );
            // 首条 user turn 自动设置 title
            assert_eq!(state.session.title, "what is Rust?", "首条 user turn 应自动 set_title");
            assert!(has_log_trace(&effects));
        }

        /// Step4-8b: RecordUserTurn 第二条时不覆盖已有 title
        #[test]
        fn test_redux_record_user_turn_no_overwrite_existing_title() {
            let mut state = s();
            state.session.title = "My Chat".to_string();
            let _ = state.reduce(Action::RecordUserTurn("second question".to_string()));
            assert_eq!(state.session.title, "My Chat", "已有 title 不应被覆盖");
        }

        /// Step4-9: RecordAssistantTurn → session.turns + history 增长，updated_at 更新
        #[test]
        fn test_redux_record_assistant_turn_grows_history() {
            let mut state = s();
            let _ = state.reduce(Action::RecordUserTurn("hello".to_string()));
            let effects = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "Rust is fast.".to_string(),
            });
            assert_eq!(state.session.turns.len(), 2, "turns 增长至 2");
            assert_eq!(state.session.history.len(), 2, "history 增长至 2");
            assert_eq!(
                state.session.turns.last().map(|t| t.role.as_str()),
                Some("assistant"),
                "turns.last() role"
            );
            assert_eq!(
                state.session.history.last().map(|m| m.role.as_str()),
                Some("assistant"),
                "history.last() role"
            );
            assert_eq!(
                state.session.history.last().map(|m| m.content.as_str()),
                Some("Rust is fast."),
                "history.last() content"
            );
            assert!(has_log_trace(&effects));
        }

        /// Step4-10: HistoryCleared — 保留 system prompt，清空 user/assistant
        #[test]
        fn test_redux_history_cleared_keeps_system_prompt() {
            let mut state = s();
            // 构造 system + user + assistant
            state
                .session
                .history
                .push(crate::providers::ChatMessage::system("Be helpful"));
            state.session.history.push(crate::providers::ChatMessage::user("hi"));
            state
                .session
                .history
                .push(crate::providers::ChatMessage::assistant("hello!"));
            state
                .ui
                .conversation_lines
                .push(crate::chat::tui::ConversationLine::User {
                    content: "hi".to_string(),
                });
            assert_eq!(state.session.history.len(), 3);

            let effects = state.reduce(Action::HistoryCleared);

            // history 只剩 system prompt
            assert_eq!(state.session.history.len(), 1, "清除后应只保留 system prompt");
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("system"),
                "保留的 history[0] 应为 system"
            );
            // conversation_lines 清空
            assert!(state.ui.conversation_lines.is_empty(), "UI conversation_lines 应清空");
            assert!(has_request_redraw(&effects));
            assert!(has_log_trace(&effects));
        }

        /// Step4-10b: HistoryCleared 无 system prompt — 全清
        #[test]
        fn test_redux_history_cleared_no_system_prompt_clears_all() {
            let mut state = s();
            state.session.history.push(crate::providers::ChatMessage::user("q"));
            state
                .session
                .history
                .push(crate::providers::ChatMessage::assistant("a"));
            let effects = state.reduce(Action::HistoryCleared);
            assert!(state.session.history.is_empty(), "无 system prompt 应完全清空");
            assert!(has_request_redraw(&effects));
        }

        /// P2-D: HistoryCleared system 不在首位 — 仍能保留（防御性全扫描）
        #[test]
        fn test_redux_history_cleared_preserves_system_in_middle() {
            let mut state = s();
            // 故意把 system 放中间（非正常顺序，但防御性处理）
            state.session.history.push(crate::providers::ChatMessage::user("q1"));
            state
                .session
                .history
                .push(crate::providers::ChatMessage::system("Be helpful"));
            state
                .session
                .history
                .push(crate::providers::ChatMessage::assistant("a1"));
            assert_eq!(state.session.history.len(), 3);

            let _effects = state.reduce(Action::HistoryCleared);

            // system 消息应被保留，user/assistant 清除
            assert_eq!(state.session.history.len(), 1, "应只保留 1 条 system 消息");
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("system"),
                "保留的应为 system 消息"
            );
        }

        /// Step4-11: 完整双 Ctrl+C 链路（含 Effect 序列）
        ///
        /// t=100: KeyPressed(Ctrl+C) → 单击，last_ctrlc_ms=100, no Quit
        /// t=300: KeyPressed(Ctrl+C) → 双击(<500ms), → Quit effect
        #[test]
        fn test_redux_double_ctrl_c_flow_e2e() {
            let mut state = s();
            let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

            // 第一次 Ctrl+C at t=100
            let effects1 = state.reduce_with_now(Action::KeyPressed(key.clone()), 100);
            assert!(!has_quit(&effects1), "第一次 Ctrl+C 不应 Quit");
            assert_eq!(state.ui.last_ctrlc_ms, 100, "记录窗口时间戳");

            // 第二次 Ctrl+C at t=300（100ms 内）
            let effects2 = state.reduce_with_now(Action::KeyPressed(key), 300);
            assert!(has_quit(&effects2), "300ms 内双击 Ctrl+C 应产生 Quit");

            // 验证 Effect::Quit 在结果中
            let has_quit_effect = effects2.iter().any(|e| matches!(e, Effect::Quit));
            assert!(has_quit_effect, "effects2 应包含 Effect::Quit");
        }

        /// S2-B Step 1: HistoryCompacted 算法基线 — 保留 system + 截断单条 + 限总预算
        #[test]
        fn test_redux_history_compacted_basic_algorithm() {
            use crate::chat::action::CompactReason;
            let mut state = s();
            // 构造 1 个 system + 20 条 user/assistant 长消息
            state
                .session
                .history
                .push(crate::providers::ChatMessage::system("system prompt - keep me"));
            for i in 0..20 {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                let content = "x".repeat(500); // 超 COMPACT_CONTENT_CHARS=320
                state.session.history.push(crate::providers::ChatMessage {
                    role: role.to_string(),
                    content: format!("{content} #{i}"),
                });
            }
            let before_len = state.session.history.len();
            assert_eq!(before_len, 21);

            let effects = state.reduce(Action::HistoryCompacted {
                reason: CompactReason::ContextOverflow,
            });

            // system prompt 必须保留在首位
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("system"),
                "compaction 后 system 仍在首位"
            );
            // 总条数应 <= 1 system + COMPACT_KEEP_MESSAGES
            assert!(
                state.session.history.len() <= 1 + super::COMPACT_KEEP_MESSAGES,
                "compaction 后非 system 条数 ≤ COMPACT_KEEP_MESSAGES, got {}",
                state.session.history.len()
            );
            // 每条非 system 消息字符数 ≤ COMPACT_CONTENT_CHARS（+ "..." 后 +3）
            for m in state.session.history.iter().skip(1) {
                assert!(
                    m.content.chars().count() <= super::COMPACT_CONTENT_CHARS + 3,
                    "non-system msg should be truncated, got {} chars",
                    m.content.chars().count()
                );
            }
            // 总预算（非 system）应 ≤ COMPACT_TOTAL_CHARS
            let non_system_chars: usize = state
                .session
                .history
                .iter()
                .skip(1)
                .map(|m| m.content.chars().count())
                .sum();
            assert!(
                non_system_chars <= super::COMPACT_TOTAL_CHARS,
                "non-system total chars {non_system_chars} > budget {}",
                super::COMPACT_TOTAL_CHARS
            );
            // 必发 LogTrace
            assert!(has_log_trace(&effects), "HistoryCompacted 必须发 LogTrace");
        }

        /// S2-B Step 1: HistoryCompacted 在 len<=1 时是 no-op
        #[test]
        fn test_redux_history_compacted_noop_when_short() {
            use crate::chat::action::CompactReason;
            let mut state = s();
            // 仅 1 条 system → 无可压缩
            state
                .session
                .history
                .push(crate::providers::ChatMessage::system("only system"));
            let effects = state.reduce(Action::HistoryCompacted {
                reason: CompactReason::Manual,
            });
            assert_eq!(state.session.history.len(), 1, "len<=1 时不变");
            assert!(has_log_trace(&effects), "no-op 仍发 LogTrace(DEBUG)");
        }

        fn messages_as_pairs(messages: &[crate::providers::ChatMessage]) -> Vec<(String, String)> {
            messages
                .iter()
                .map(|message| (message.role.clone(), message.content.clone()))
                .collect()
        }

        #[test]
        fn redux_compaction_patch_applies_exactly_and_matches_driver_history() {
            use crate::chat::action::CompactReason;
            let mut state = s();
            state.session.history = vec![
                crate::providers::ChatMessage::system("sys"),
                crate::providers::ChatMessage::user("old user"),
                crate::providers::ChatMessage::assistant("old assistant"),
                crate::providers::ChatMessage::user("recent user"),
            ];
            let mut driver_history = state.session.history.clone();
            let guard = crate::agent::loop_::compaction_patch_guard_for(&driver_history, 1, 3).expect("guard");
            let patch = crate::agent::loop_::CompactionPatch {
                range_start: 1,
                range_end: 3,
                replacement: vec![crate::providers::ChatMessage::assistant(
                    "[Context compacted at test. Summary: PROVIDER_SUMMARY_MARKER]",
                )],
                append_after: vec![crate::providers::ChatMessage::user(
                    "[Post-compaction context refresh]\nre-read",
                )],
                guard,
            };
            let config = crate::config::AgentCompactionConfig {
                max_context_tokens: 10_000,
                reserve_tokens: 10,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };

            crate::agent::loop_::apply_compaction_patch_exact(&mut driver_history, &patch);
            let effects = state.reduce(Action::HistoryCompactionPatchApplied {
                reason: CompactReason::ContextOverflow,
                patch,
                compaction_config: config,
            });

            assert_eq!(
                messages_as_pairs(&state.session.history),
                messages_as_pairs(&driver_history),
                "GP-6: reducer history must exactly match driver history after patch"
            );
            assert!(
                state
                    .session
                    .history
                    .iter()
                    .any(|message| message.content.contains("PROVIDER_SUMMARY_MARKER")),
                "provider summary marker must be present"
            );
            assert!(has_log_trace(&effects));
        }

        #[test]
        fn compaction_patch_refresh_position_parity_between_legacy_and_redux() {
            use crate::chat::action::CompactReason;
            let current_question = "What should ISS-037 answer now?";
            let mut state = s();
            state.session.history = vec![
                crate::providers::ChatMessage::system("sys"),
                crate::providers::ChatMessage::user("old user"),
                crate::providers::ChatMessage::assistant("old assistant"),
                crate::providers::ChatMessage::user(current_question),
            ];
            let mut legacy_history = state.session.history.clone();
            let guard = crate::agent::loop_::compaction_patch_guard_for(&legacy_history, 1, 3).expect("guard");
            let patch = crate::agent::loop_::CompactionPatch {
                range_start: 1,
                range_end: 3,
                replacement: vec![crate::providers::ChatMessage::assistant(
                    "[Context compacted at test. Summary: ISS-037 parity summary]",
                )],
                append_after: vec![crate::providers::ChatMessage::user(
                    "[Post-compaction context refresh]\nre-read",
                )],
                guard,
            };
            let config = crate::config::AgentCompactionConfig {
                max_context_tokens: 10_000,
                reserve_tokens: 10,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };

            crate::agent::loop_::apply_compaction_patch_exact(&mut legacy_history, &patch);
            let _ = state.reduce(Action::HistoryCompactionPatchApplied {
                reason: CompactReason::ContextOverflow,
                patch,
                compaction_config: config,
            });

            assert_eq!(
                messages_as_pairs(&state.session.history),
                messages_as_pairs(&legacy_history),
                "GP-6: legacy and Redux histories must match exactly after the shared patch primitive"
            );
            let refresh_index = state
                .session
                .history
                .iter()
                .position(|message| message.content.starts_with("[Post-compaction context refresh]"))
                .expect("refresh marker should be present");
            let summary_index = state
                .session
                .history
                .iter()
                .position(|message| message.content.contains("ISS-037 parity summary"))
                .expect("summary marker should be present");
            let question_index = state
                .session
                .history
                .iter()
                .position(|message| message.content == current_question)
                .expect("current question should be present");
            assert!(
                summary_index < refresh_index && refresh_index < question_index,
                "refresh marker must sit after the summary and before the real current question"
            );
            assert_eq!(
                state.session.history.last().map(|message| message.content.as_str()),
                Some(current_question),
                "real current user question must remain the trailing provider-bound user message"
            );
        }

        #[test]
        fn post_compaction_refresh_not_persisted_as_session_turn() {
            use crate::chat::action::CompactReason;
            let current_question = "Persist this as the real user turn";
            let assistant_reply = "assistant reply bound to the real user turn";
            let mut state = s();
            state.session.history = vec![
                crate::providers::ChatMessage::system("sys"),
                crate::providers::ChatMessage::user("old user"),
                crate::providers::ChatMessage::assistant("old assistant"),
            ];
            let _ = state.reduce(Action::RecordUserTurn(current_question.to_string()));
            let guard = crate::agent::loop_::compaction_patch_guard_for(&state.session.history, 1, 3).expect("guard");
            let patch = crate::agent::loop_::CompactionPatch {
                range_start: 1,
                range_end: 3,
                replacement: vec![crate::providers::ChatMessage::assistant(
                    "[Context compacted at test. Summary: persisted-turn summary]",
                )],
                append_after: vec![crate::providers::ChatMessage::user(
                    "[Post-compaction context refresh]\nre-read",
                )],
                guard,
            };
            let config = crate::config::AgentCompactionConfig {
                max_context_tokens: 10_000,
                reserve_tokens: 10,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };

            let _ = state.reduce(Action::HistoryCompactionPatchApplied {
                reason: CompactReason::ContextOverflow,
                patch,
                compaction_config: config,
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: assistant_reply.to_string(),
            });

            assert_eq!(
                state.session.turns.len(),
                3,
                "the compaction summary, real user, and assistant turns are persisted"
            );
            let [summary_turn, user_turn, assistant_turn] = state.session.turns.as_slice() else {
                panic!("expected summary, real user turn, and assistant turn");
            };
            assert_eq!(summary_turn.role, "assistant");
            assert!(summary_turn.content.contains("persisted-turn summary"));
            assert_eq!(user_turn.role, "user");
            assert_eq!(user_turn.content, current_question);
            assert_eq!(assistant_turn.role, "assistant");
            assert_eq!(assistant_turn.content, assistant_reply);
            assert!(
                state
                    .session
                    .turns
                    .iter()
                    .all(|turn| !turn.content.starts_with("[Post-compaction context refresh]")),
                "refresh marker must remain a history context marker, not a persisted user turn"
            );

            let snapshot = state.build_session_snapshot();
            let mut reloaded = s();
            let _ = reloaded.reduce(Action::SessionLoaded(snapshot));
            assert_eq!(
                messages_as_pairs(&reloaded.session.history),
                vec![
                    (
                        "assistant".to_string(),
                        "[Context compacted at test. Summary: persisted-turn summary]".to_string()
                    ),
                    ("user".to_string(), current_question.to_string()),
                    ("assistant".to_string(), assistant_reply.to_string()),
                ],
                "resume must rebuild the compacted durable turn shape"
            );
        }

        #[test]
        fn redux_compaction_patch_guard_mismatch_falls_back_without_stale_patch() {
            use crate::chat::action::CompactReason;
            let mut state = s();
            let original = vec![
                crate::providers::ChatMessage::system("sys"),
                crate::providers::ChatMessage::user(format!("old user {}", "x ".repeat(180))),
                crate::providers::ChatMessage::assistant(format!("old assistant {}", "y ".repeat(180))),
                crate::providers::ChatMessage::user(format!("recent {}", "z ".repeat(180))),
            ];
            let guard = crate::agent::loop_::compaction_patch_guard_for(&original, 1, 3).expect("guard");
            state.session.history = original;
            state
                .session
                .history
                .push(crate::providers::ChatMessage::user("mutation before reducer"));
            let patch = crate::agent::loop_::CompactionPatch {
                range_start: 1,
                range_end: 3,
                replacement: vec![crate::providers::ChatMessage::assistant(
                    "[Context compacted at test. Summary: STALE_PROVIDER_SUMMARY]",
                )],
                append_after: vec![crate::providers::ChatMessage::user("stale refresh")],
                guard,
            };
            let config = crate::config::AgentCompactionConfig {
                max_context_tokens: 90,
                reserve_tokens: 10,
                max_context_tokens_explicit: true,
                ..crate::config::AgentCompactionConfig::default()
            };

            let effects = state.reduce(Action::HistoryCompactionPatchApplied {
                reason: CompactReason::ContextOverflow,
                patch,
                compaction_config: config.clone(),
            });

            assert!(
                state
                    .session
                    .history
                    .iter()
                    .all(|message| !message.content.contains("STALE_PROVIDER_SUMMARY")),
                "guard mismatch must not apply stale provider patch"
            );
            assert!(
                crate::agent::loop_::plan_context_budget(
                    &state.session.history,
                    &config,
                    crate::agent::loop_::PRE_TURN_FLUSH_THRESHOLD
                )
                .used_tokens
                    <= 80,
                "fallback trim must bring history under literal hard limit 80"
            );
            assert!(effects.iter().any(|effect| matches!(
                effect,
                Effect::LogTrace {
                    level: tracing::Level::WARN,
                    msg
                } if msg.contains("guard mismatch")
            )));
        }

        /// Step4-12: CancelRequested 后再 CancelRequested — 第二次 no-op（generating=false）
        #[test]
        fn test_redux_cancel_requested_twice_second_noop() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d1".to_string(),
                cancel: CancellationToken::new(),
            });
            // 第一次取消
            let effects1 = state.reduce(Action::CancelRequested);
            assert!(!effects1.is_empty(), "第一次取消应有 effects");
            // 第二次取消 — generating 已 false → no-op
            let effects2 = state.reduce(Action::CancelRequested);
            assert!(effects2.is_empty(), "第二次 CancelRequested(generating=false) 应 no-op");
        }
    }

    // ─── Step 5a-3 Phase A + F 测试 ────────────────────────────────────────────
    //
    // Phase A: StartLLMTurn 真主导路径 — reducer 初始化 draft + 同时发射 Effect::StartTurn
    // Phase F: StreamFailed 真发 NotifyHook(Error)；StreamCancelled 不发 hook 也不 SaveSession

    #[cfg(test)]
    mod phase_a_f {
        use super::super::*;
        use crate::chat::action::Action;
        use crate::providers::ChatMessage;
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        fn has_start_turn(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::StartTurn { .. }))
        }
        fn has_notify_hook(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::NotifyHook { .. }))
        }
        fn has_save_session(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::SaveSession(_)))
        }
        fn has_request_redraw(effects: &[Effect]) -> bool {
            effects.iter().any(|e| matches!(e, Effect::RequestRedraw))
        }

        /// Phase A-1: StartLLMTurn 初始化 draft + 同步发射 Effect::StartTurn(携带 history)
        #[test]
        fn test_phase_a_start_llm_turn_emits_effect_start_turn() {
            let mut state = s();
            let cancel = CancellationToken::new();
            let history = vec![ChatMessage::system("you are helpful"), ChatMessage::user("hi")];

            let effects = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: None,
                provider_turn_sequence: None,
                draft_id: "draft-1".to_string(),
                history,
                compaction_config: None,
                cancel,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
            });

            // 状态变更：draft + active_cancel + generating
            assert!(
                state.stream.primary_streaming_draft().is_some(),
                "stream.draft 必须被设置"
            );
            assert!(state.control.active_cancel.is_some(), "active_cancel 必须被注册");
            assert!(state.control.generating, "generating 必须置 true");

            // Effect 验证
            assert!(has_start_turn(&effects), "必须发射 Effect::StartTurn");
            assert!(has_request_redraw(&effects), "必须发射 Effect::RequestRedraw");

            // history 必须穿透到 Effect::StartTurn
            let history_in_effect = effects.iter().find_map(|e| match e {
                Effect::StartTurn { history, draft_id, .. } => Some((draft_id.clone(), history.clone())),
                _ => None,
            });
            let (draft_id, hist) = history_in_effect.expect("StartTurn effect 必须存在");
            assert_eq!(draft_id, "draft-1");
            assert_eq!(hist.len(), 2);
            let h0 = hist.first().expect("history[0] 必须存在");
            let h1 = hist.get(1).expect("history[1] 必须存在");
            assert_eq!(h0.role, "system");
            assert_eq!(h1.role, "user");
        }

        /// Phase A-2: StartLLMTurn 注册的 cancel 与 Effect::StartTurn 中的 cancel 是同一个 token
        #[test]
        fn test_phase_a_start_llm_turn_cancel_propagates() {
            let mut state = s();
            let cancel = CancellationToken::new();
            let effects = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: None,
                provider_turn_sequence: None,
                draft_id: "d2".to_string(),
                history: vec![ChatMessage::user("x")],
                compaction_config: None,
                cancel: cancel.clone(),
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
            });
            // 通过取消原 token，验证 Effect 内的 token 一并取消（共享 cancellation）
            cancel.cancel();
            let cancel_in_effect = effects.iter().find_map(|e| match e {
                Effect::StartTurn { cancel, .. } => Some(cancel.clone()),
                _ => None,
            });
            let tok = cancel_in_effect.expect("StartTurn 必须携带 cancel");
            assert!(tok.is_cancelled(), "StartTurn 中的 cancel 应与原 token 共享");
        }

        #[test]
        fn start_llm_turn_carries_compaction_config_to_effect() {
            let mut state = s();
            let compaction_config = crate::config::AgentCompactionConfig {
                max_context_tokens: 120,
                reserve_tokens: 10,
                max_context_tokens_explicit: true,
                memory_flush: false,
                ..crate::config::AgentCompactionConfig::default()
            };

            let effects = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: None,
                provider_turn_sequence: None,
                draft_id: "d-budget".to_string(),
                history: vec![ChatMessage::user("x")],
                compaction_config: Some(compaction_config),
                cancel: CancellationToken::new(),
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
            });

            let carried = effects.iter().find_map(|effect| match effect {
                Effect::StartTurn {
                    provider_turn_task_id: None,
                    compaction_config: Some(config),
                    ..
                } => Some(config),
                _ => None,
            });
            let config = carried.expect("StartTurn effect must carry compaction config");
            assert_eq!(config.max_context_tokens, 120);
            assert_eq!(config.reserve_tokens, 10);
        }

        #[test]
        fn start_llm_turn_carries_provider_turn_task_id_to_effect() {
            let mut state = s();
            let mut scheduler = crate::chat::turn_scheduler::TurnScheduler::new();
            let task_id = scheduler.enqueue(
                "provider identity",
                crate::chat::turn_scheduler::TurnPriority::Normal,
                0,
            );

            let effects = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: Some(task_id),
                provider_turn_sequence: None,
                draft_id: "d-worker".to_string(),
                history: vec![ChatMessage::user("x")],
                compaction_config: None,
                cancel: CancellationToken::new(),
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
            });

            let carried = effects.iter().find_map(|effect| match effect {
                Effect::StartTurn {
                    provider_turn_task_id, ..
                } => Some(*provider_turn_task_id),
                _ => None,
            });
            assert_eq!(carried, Some(Some(task_id)));
        }

        /// Phase A-3: TurnStarted（旧 Action）保持原行为 — 不发射 Effect::StartTurn
        #[test]
        fn test_phase_a_legacy_turn_started_no_start_turn_effect() {
            let mut state = s();
            let effects = state.reduce(Action::TurnStarted {
                draft_id: "legacy".to_string(),
                cancel: CancellationToken::new(),
            });
            assert!(
                state.stream.primary_streaming_draft().is_some(),
                "TurnStarted 同样初始化 draft"
            );
            assert!(
                !has_start_turn(&effects),
                "TurnStarted 不应发 Effect::StartTurn（旧路径仍由 chat::run 主导）"
            );
        }

        /// Phase F-1: StreamFailed 发射 NotifyHook(Error) — 与旧路径 hooks.emit(HookEvent::Error) 对齐
        #[test]
        fn test_phase_f_stream_failed_emits_notify_hook() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d3".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamFailed {
                draft_id: "d3".to_string(),
                err: "boom".to_string(),
                retryable: false,
            });
            assert!(has_notify_hook(&effects), "StreamFailed 必须发 NotifyHook(Error)");
            let hook_evt = effects.iter().find_map(|e| match e {
                Effect::NotifyHook { event, payload } => Some((*event, payload.clone())),
                _ => None,
            });
            let (evt, payload) = hook_evt.expect("NotifyHook 必须存在");
            assert!(matches!(evt, HookEvent::Error));
            assert_eq!(payload.get("component").and_then(|v| v.as_str()), Some("chat-turn"));
            assert_eq!(payload.get("message").and_then(|v| v.as_str()), Some("boom"));
            assert_eq!(
                payload.get("retryable").and_then(serde_json::Value::as_bool),
                Some(false)
            );
        }

        /// Phase F-2: StreamCancelled 不发 NotifyHook 也不 SaveSession（中断 turn 不写持久化、不双触发钩子）
        #[test]
        fn test_phase_f_stream_cancelled_no_save_no_hook() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d4".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "d4".to_string(),
            });
            assert!(!has_notify_hook(&effects), "StreamCancelled 不应发 NotifyHook");
            assert!(!has_save_session(&effects), "StreamCancelled 不应 SaveSession");
            assert!(has_request_redraw(&effects), "StreamCancelled 仍需 RequestRedraw");
        }

        /// Phase F-3: 不匹配 draft_id 的 StreamFailed → no-op，不发 NotifyHook（防止 stale 误报）
        #[test]
        fn test_phase_f_stream_failed_wrong_id_no_hook() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "right".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamFailed {
                draft_id: "wrong".to_string(),
                err: "stale".to_string(),
                retryable: true,
            });
            assert!(effects.is_empty(), "stale draft_id 应 no-op");
            assert!(!has_notify_hook(&effects));
        }

        /// Phase A-4: StartLLMTurn 后立刻取消 — 状态正确清理（generating=true → cancel token 也准备好让执行器收到 cancelled）
        #[test]
        fn test_phase_a_start_llm_turn_then_cancel_request() {
            let mut state = s();
            let cancel = CancellationToken::new();
            let _ = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: None,
                provider_turn_sequence: None,
                draft_id: "d5".to_string(),
                history: vec![ChatMessage::user("hi")],
                compaction_config: None,
                cancel,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
            });
            assert!(state.control.generating);

            let effects = state.reduce(Action::CancelRequested);
            // generating=true → reducer 发 CancelDraft
            assert!(
                effects.iter().any(|e| matches!(e, Effect::CancelDraft(_))),
                "CancelRequested(generating=true) 应发 CancelDraft"
            );
            assert!(!state.control.generating, "cancel 后 generating 必须复位");
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "cancel 后 draft 必须清理"
            );
        }

        fn start_phase1_draft(state: &mut ChatState, draft_id: &str, sequence: u64, prompt: &str) {
            let effects = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: None,
                provider_turn_sequence: Some(sequence),
                draft_id: draft_id.to_string(),
                history: vec![ChatMessage::user(prompt)],
                compaction_config: None,
                cancel: CancellationToken::new(),
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
            });
            assert!(has_start_turn(&effects), "phase1 draft start must still emit StartTurn");
        }

        fn visible_draft_ids(state: &ChatState) -> Vec<&str> {
            state
                .stream
                .visible_drafts
                .iter()
                .map(|turn| turn.draft.draft_id.as_str())
                .collect()
        }

        fn draft_text(state: &ChatState, draft_id: &str) -> Option<String> {
            state
                .stream
                .visible_drafts
                .iter()
                .find(|turn| turn.draft.draft_id == draft_id)
                .map(|turn| turn.draft.accumulated.clone())
        }

        fn provider_worker_status(sequences: &[u64]) -> ProviderWorkerStatus {
            ProviderWorkerStatus {
                running: sequences.len(),
                cancelling: 0,
                awaiting_commit: 0,
                finalized_payloads: 0,
                finalized_total_tokens: 0,
                oldest_started_at_ms: Some(0),
                rows: sequences
                    .iter()
                    .map(|sequence| crate::chat::action::ProviderWorkerStatusRow {
                        task_id: *sequence,
                        sequence: *sequence,
                        kind: crate::chat::action::ProviderWorkerRowKind::Detached,
                        state: crate::chat::action::ProviderWorkerRowState::Running,
                        started_at_ms: 0,
                        finalized_total_tokens: None,
                        completion_ready: false,
                    })
                    .collect(),
            }
        }

        fn active_worker_view_text(state: &ChatState) -> String {
            state
                .ui
                .active_session_view
                .as_ref()
                .map(|view| view.lines.join("\n"))
                .unwrap_or_default()
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn phase2_snapshot_exposes_worker_drafts_and_keeps_primary_streaming() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-a".to_string(),
                delta: "A live".to_string(),
                version: 1,
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-b".to_string(),
                delta: "B live".to_string(),
                version: 1,
            });

            let snapshot = state.build_ui_snapshot(42);

            assert_eq!(
                snapshot.streaming.as_ref().map(|draft| draft.draft_id.as_str()),
                Some("draft-a")
            );
            assert_eq!(
                snapshot
                    .streaming_draft_for_worker(20)
                    .map(|draft| draft.accumulated.as_str()),
                Some("B live")
            );
            assert!(snapshot.streaming_draft_for_worker(30).is_none());
            assert_eq!(
                snapshot
                    .visible_streaming_drafts
                    .iter()
                    .map(|draft| draft.sequence)
                    .collect::<Vec<_>>(),
                vec![10, 20]
            );
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn phase2_worker_pane_focus_uses_matching_draft_not_primary() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-a".to_string(),
                delta: "A live".to_string(),
                version: 1,
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-b".to_string(),
                delta: "B live".to_string(),
                version: 1,
            });

            state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 10 };
            let _ = state.reduce(Action::ProviderWorkerStatusUpdated {
                status: provider_worker_status(&[10, 20]),
            });
            let view_a = active_worker_view_text(&state);
            assert!(view_a.contains("assistant streaming: A live"), "{view_a}");
            assert!(!view_a.contains("B live"), "{view_a}");

            state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 20 };
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-b".to_string(),
                delta: " B2".to_string(),
                version: 2,
            });
            let view_b = active_worker_view_text(&state);
            assert!(view_b.contains("assistant streaming: B live B2"), "{view_b}");
            assert!(!view_b.contains("A live"), "{view_b}");
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn phase2_worker_pane_missing_draft_uses_empty_io_not_history_or_primary() {
            let mut state = s();
            state.ui.conversation_lines.push(ConversationLine::User {
                content: "history user".to_string(),
            });
            state.ui.conversation_lines.push(ConversationLine::Assistant {
                content: "history assistant must not leak".to_string(),
            });
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-a".to_string(),
                delta: "primary live must not leak".to_string(),
                version: 1,
            });

            state.ui.focus = crate::chat::sessions::FocusTarget::Worker { sequence: 30 };
            let _ = state.reduce(Action::ProviderWorkerStatusUpdated {
                status: provider_worker_status(&[30]),
            });
            let view = active_worker_view_text(&state);

            assert!(!view.contains("io: recent provider turn"), "{view}");
            assert!(!view.contains("history assistant must not leak"), "{view}");
            assert!(!view.contains("primary live must not leak"), "{view}");
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn phase2_main_transcript_primary_streaming_path_is_unchanged() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-b", 20, "second");
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            let snapshot = state.build_ui_snapshot(1);

            assert_eq!(
                snapshot.streaming.as_ref().map(|draft| draft.draft_id.as_str()),
                Some("draft-a")
            );
            assert_eq!(
                state
                    .stream
                    .primary_streaming_draft()
                    .map(|draft| draft.draft_id.as_str()),
                Some("draft-a")
            );
        }

        #[test]
        fn phase1_two_visible_drafts_start_without_overwriting() {
            let mut state = s();

            start_phase1_draft(&mut state, "draft-b", 20, "second prompt");
            start_phase1_draft(&mut state, "draft-a", 10, "first prompt");

            assert_eq!(visible_draft_ids(&state), vec!["draft-a", "draft-b"]);
            assert_eq!(
                state
                    .stream
                    .primary_draft()
                    .map(|turn| (turn.sequence, turn.prompt_preview.as_str())),
                Some((10, "first prompt"))
            );
            assert!(state.control.generating);
        }

        #[test]
        fn phase1_stream_chunks_route_by_draft_id() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");

            let b_effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-b".to_string(),
                delta: "B1".to_string(),
                version: 1,
            });
            let a_effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-a".to_string(),
                delta: "A1".to_string(),
                version: 1,
            });

            assert!(has_request_redraw(&b_effects));
            assert!(has_request_redraw(&a_effects));
            assert_eq!(draft_text(&state, "draft-a"), Some("A1".to_string()));
            assert_eq!(draft_text(&state, "draft-b"), Some("B1".to_string()));
            assert_eq!(visible_draft_ids(&state), vec!["draft-a", "draft-b"]);
        }

        #[test]
        fn phase1_stream_completed_removes_only_matching_draft() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");

            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "draft-a".to_string(),
                final_text: "answer a".to_string(),
                reasoning: String::new(),
            });

            assert!(has_save_session(&effects));
            assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
            assert!(
                state.control.generating,
                "remaining draft keeps structural generating state"
            );
            assert_eq!(
                state
                    .stream
                    .primary_streaming_draft()
                    .map(|draft| draft.draft_id.as_str()),
                Some("draft-b")
            );
        }

        #[test]
        fn phase1_stale_chunk_for_completed_draft_is_ignored() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "draft-a".to_string(),
                final_text: "answer a".to_string(),
                reasoning: String::new(),
            });

            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-a".to_string(),
                delta: "late".to_string(),
                version: 1,
            });

            assert!(effects.is_empty(), "completed draft must reject late chunks");
            assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
            assert_eq!(draft_text(&state, "draft-b"), Some(String::new()));
        }

        #[test]
        fn phase1_stream_cancelled_removes_only_matching_draft() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");

            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "draft-a".to_string(),
            });

            assert!(has_request_redraw(&effects));
            assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
            assert!(
                state.control.generating,
                "cancelling one structural draft must not stop the other"
            );
        }

        #[test]
        fn phase1_stream_failed_removes_only_matching_draft() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");

            let effects = state.reduce(Action::StreamFailed {
                draft_id: "draft-a".to_string(),
                err: "failed a".to_string(),
                retryable: false,
            });

            assert!(has_notify_hook(&effects));
            assert_eq!(visible_draft_ids(&state), vec!["draft-b"]);
            assert!(
                state.control.generating,
                "failing one structural draft must not stop the other"
            );
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn phase1_snapshot_dirty_changes_when_non_primary_draft_version_changes() {
            let mut state = s();
            start_phase1_draft(&mut state, "draft-a", 10, "first");
            start_phase1_draft(&mut state, "draft-b", 20, "second");
            let before = state.snapshot_dirty_fields();

            let effects = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-b".to_string(),
                delta: "B1".to_string(),
                version: 1,
            });
            let after = state.snapshot_dirty_fields();

            assert!(has_request_redraw(&effects));
            assert_ne!(
                before, after,
                "non-primary draft version must affect snapshot dirty fingerprint"
            );
            assert_eq!(draft_text(&state, "draft-b"), Some("B1".to_string()));
            assert_eq!(
                state
                    .stream
                    .primary_streaming_draft()
                    .map(|draft| draft.draft_id.as_str()),
                Some("draft-a"),
                "non-primary chunk must not change primary selection"
            );
        }

        // ─── S2-A: chat::run stream-path → Redux dispatch wiring tests ─────
        //
        // 这四个测试覆盖 chat::mod 的流式路径接入 Redux dispatch 后的契约：
        //   1. 双写一致性（M2 验收点）：同一 delta 序列下，旧路径 `update_draft`
        //      传给 terminal 的 `accumulated` 文本 == reducer `stream.draft.accumulated`。
        //      reducer 通过 StreamChunkReceived 累积；旧路径通过 push_str 累积；
        //      两者必须字节级相同。
        //   2. StreamCompleted Effect 序列：含 NotifyHook(TurnComplete) + RequestRedraw。
        //   3. StreamFailed Effect 序列：含 LogTrace(WARN) + NotifyHook(Error) + RequestRedraw。
        //   4. StreamCancelled Effect 序列：仅 RequestRedraw（不发 hook），且 cancel
        //      在失败分类**之前**判别（避免误发 Failed）。

        /// S2-A test 1: draft_text_consistency_legacy_vs_redux
        ///
        /// 复现 chat::mod 主循环 `draft_updater` 任务的 delta 累积语义：
        ///   - 旧路径：`accumulated.push_str(&delta); update_draft(accumulated)` — 传累计
        ///   - 新路径：`coalescer.try_send_chunk(draft_id, delta, version)` → reducer
        ///     `reduce_stream_chunk_received` 通过 `draft.accumulated.push_str(delta)` 累积
        ///
        /// 在 fast-path（coalescer 不背压）下两者必须字节级一致。
        #[test]
        fn test_s2a_draft_text_consistency_legacy_vs_redux() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-consistency".to_string(),
                cancel: CancellationToken::new(),
            });

            // 模拟 SSE 流式 deltas（含 emoji / 多字节字符，验证字节级一致性）
            let deltas: [&str; 6] = ["Hel", "lo, ", "wo", "rld", " 你好", " 🌍"];
            let mut legacy_accumulated = String::new();
            let mut version: u64 = 0;
            for delta in &deltas {
                // 旧路径累积语义：accumulated.push_str + update_draft(accumulated)
                legacy_accumulated.push_str(delta);
                // 新路径：dispatch 增量 delta（非累计串）
                version = version.saturating_add(1);
                let _ = state.reduce(Action::StreamChunkReceived {
                    draft_id: "draft-consistency".to_string(),
                    delta: (*delta).to_string(),
                    version,
                });
            }

            // 核心验收点：旧路径 accumulated == reducer 内部 accumulated
            let redux_accumulated = state
                .stream
                .primary_streaming_draft()
                .map(|d| d.accumulated.clone())
                .expect("test: stream.draft must exist after StreamChunkReceived");
            assert_eq!(
                redux_accumulated, legacy_accumulated,
                "S2-A M2 验收：reducer accumulated 必须等于旧路径 update_draft 传入的累计串"
            );
            assert_eq!(
                state.stream.primary_streaming_draft().map(|d| d.version),
                Some(version),
                "reducer version 必须等于 draft_updater 内 counter 终值"
            );
        }

        /// S2-A test 2: stream_completed_effect_sequence
        ///
        /// Success 路径：chat::mod 主循环按 S2-A 改造后投递
        ///   `Action::StreamCompleted { draft_id, final_text, reasoning: "" }`
        /// 期望 reducer 发射 `[NotifyHook(TurnComplete), RequestRedraw]` 且 draft 清理。
        #[test]
        fn test_s2a_stream_completed_effect_sequence() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-completed".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-completed".to_string(),
                delta: "the answer".to_string(),
                version: 1,
            });

            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "draft-completed".to_string(),
                final_text: "the answer".to_string(),
                reasoning: String::new(),
            });

            // 终态清理
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "completed 后 draft 应清空"
            );
            assert!(!state.control.generating, "completed 后 generating=false");
            assert!(state.control.active_cancel.is_none(), "active_cancel 复位");

            // Effect 序列：NotifyHook(TurnComplete) + RequestRedraw
            let notify_turn_complete = effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::NotifyHook {
                        event: HookEvent::TurnComplete,
                        ..
                    }
                )
            });
            assert!(notify_turn_complete, "StreamCompleted 必须发 NotifyHook(TurnComplete)");
            assert!(has_request_redraw(&effects), "StreamCompleted 必须发 RequestRedraw");
        }

        /// T3-3-c-1: `StreamCompleted` 必须发 `Effect::SaveSession`（reducer 单源持久化）.
        ///
        /// 同时验证 Effect 序列契约（执行顺序：NotifyHook → SaveSession → RequestRedraw）.
        #[test]
        fn test_t3_3c_stream_completed_emits_save_session() {
            let mut state = s();
            state.session.id = "sess-T3-3c".to_string();
            // 先 record 用户 turn 让 session.turns 非空，验证快照真带 turns
            let _ = state.reduce(Action::RecordUserTurn("question".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-T3-3c".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "answer".to_string(),
            });
            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "draft-T3-3c".to_string(),
                final_text: "answer".to_string(),
                reasoning: String::new(),
            });

            // 验证 SaveSession 存在且快照内容正确
            let save_effect = effects.iter().find(|e| matches!(e, Effect::SaveSession(_)));
            assert!(
                save_effect.is_some(),
                "T3-3-c: StreamCompleted 必须发 Effect::SaveSession"
            );
            if let Some(Effect::SaveSession(snapshot)) = save_effect {
                assert_eq!(snapshot.id, "sess-T3-3c", "快照 id 应等于 session.id");
                assert_eq!(snapshot.turns.len(), 2, "快照应含 user+assistant 两条 turn");
                assert_eq!(snapshot.turns.first().map(|t| t.role.as_str()), Some("user"));
                let assistant = snapshot.turns.get(1).expect("test: turns[1] must exist");
                assert_eq!(assistant.role, "assistant");
                assert_eq!(assistant.content, "answer");
                assert_eq!(snapshot.title, "question", "auto-title 应来自首条 user turn");
                // T3-3-fixA P0-1: 显式断言 snapshot.turns 末条是当轮 assistant —
                // 固化 dispatch 顺序 (RecordAssistantTurn → StreamCompleted) 不变量
                let last = snapshot.turns.last().expect("test: snapshot.turns 必须含末条");
                assert_eq!(last.role, "assistant", "snapshot.turns.last() 必须是 assistant");
                assert_eq!(last.content, "answer", "末条 content 必须是当轮 assistant 内容");
            }

            // Effect 顺序契约：NotifyHook 在前，SaveSession 中段，RequestRedraw 收尾
            let positions: Vec<&'static str> = effects
                .iter()
                .map(|e| match e {
                    Effect::NotifyHook { .. } => "notify",
                    Effect::SaveSession(_) => "save",
                    Effect::RequestRedraw => "redraw",
                    _ => "other",
                })
                .collect();
            let notify_pos = positions.iter().position(|s| *s == "notify");
            let save_pos = positions.iter().position(|s| *s == "save");
            let redraw_pos = positions.iter().position(|s| *s == "redraw");
            assert!(
                notify_pos < save_pos && save_pos < redraw_pos,
                "Effect 顺序应为 NotifyHook < SaveSession < RequestRedraw, got: {positions:?}"
            );
        }

        /// T3-3-c-2: 重复 `StreamCompleted` 不应触发第二次 SaveSession（draft 已清空 → no-op）
        #[test]
        fn test_t3_3c_duplicate_stream_completed_no_save() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-dup".to_string(),
                cancel: CancellationToken::new(),
            });
            // 第一次 — 应含 SaveSession
            let first = state.reduce(Action::StreamCompleted {
                draft_id: "d-dup".to_string(),
                final_text: "ans".to_string(),
                reasoning: String::new(),
            });
            assert!(first.iter().any(|e| matches!(e, Effect::SaveSession(_))));
            // 第二次 — draft 已 None → 空 vec，无 SaveSession 重复
            let second = state.reduce(Action::StreamCompleted {
                draft_id: "d-dup".to_string(),
                final_text: "ans-dup".to_string(),
                reasoning: String::new(),
            });
            assert!(second.is_empty(), "重复 StreamCompleted 应是 no-op");
        }

        /// T3-3-fixA P0-1: 双向回归防护 — dispatch 顺序决定 snapshot 完整性.
        ///
        /// 正序 (RecordAssistantTurn → StreamCompleted)：snapshot.turns 含 assistant 末条.
        /// 反序 (StreamCompleted → RecordAssistantTurn)：snapshot.turns **不含** assistant,
        /// 因 SaveSession 快照在 reducer reduce_stream_completed 时同步构造,
        /// 此时 RecordAssistantTurn 尚未 push 当轮 assistant 到 session.turns.
        ///
        /// 任何未来回退 chat::run 主循环 dispatch 顺序的修改都会让本测试翻车,
        /// 把 P0-1 决策固化到 reducer 层契约里.
        #[test]
        fn t3_3_fix_a_dispatch_order_snapshot_contract() {
            // ── 正序：RecordAssistantTurn → StreamCompleted ──
            let mut state_a = s();
            state_a.session.id = "sess-fwd".to_string();
            let _ = state_a.reduce(Action::RecordUserTurn("q".to_string()));
            let _ = state_a.reduce(Action::TurnStarted {
                draft_id: "d-fwd".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state_a.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "a-fwd".to_string(),
            });
            let fwd_effects = state_a.reduce(Action::StreamCompleted {
                draft_id: "d-fwd".to_string(),
                final_text: "a-fwd".to_string(),
                reasoning: String::new(),
            });
            let fwd_snap = fwd_effects
                .iter()
                .find_map(|e| match e {
                    Effect::SaveSession(s) => Some(s),
                    _ => None,
                })
                .expect("正序：SaveSession 必发");
            let last = fwd_snap.turns.last().expect("正序：snapshot.turns 必非空");
            assert_eq!(last.role, "assistant", "正序：末条 role 必须是 assistant");
            assert_eq!(last.content, "a-fwd", "正序：末条 content 必须是当轮 assistant");

            // ── 反序：StreamCompleted → RecordAssistantTurn ──
            let mut state_b = s();
            state_b.session.id = "sess-rev".to_string();
            let _ = state_b.reduce(Action::RecordUserTurn("q".to_string()));
            let _ = state_b.reduce(Action::TurnStarted {
                draft_id: "d-rev".to_string(),
                cancel: CancellationToken::new(),
            });
            let rev_effects = state_b.reduce(Action::StreamCompleted {
                draft_id: "d-rev".to_string(),
                final_text: "a-rev".to_string(),
                reasoning: String::new(),
            });
            let _ = state_b.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "a-rev".to_string(),
            });
            let rev_snap = rev_effects
                .iter()
                .find_map(|e| match e {
                    Effect::SaveSession(s) => Some(s),
                    _ => None,
                })
                .expect("反序：SaveSession 必发");
            assert!(
                !rev_snap.turns.iter().any(|t| t.role == "assistant"),
                "反序：snapshot.turns 不应含 assistant — 这就是 P0-1 修复前的 bug 现场"
            );
            assert_eq!(rev_snap.turns.len(), 1, "反序：snapshot.turns 应只含先前的 user turn");
        }

        /// T3-3-fixA P0-2: StreamFailed 不发 SaveSession（错误路径不写持久化）.
        ///
        /// 固化附录 B 决策表中 Error 行：reduce_stream_failed emit
        /// [LogTrace, NotifyHook(Error), RequestRedraw]，无 SaveSession.
        /// 防御回归：未来若有人想"把失败也保存"，本测试立刻翻车，强制更新附录 B + 评审.
        #[test]
        fn t3_3_fix_a_stream_error_no_save() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-err".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamFailed {
                draft_id: "d-err".to_string(),
                err: "boom".to_string(),
                retryable: false,
            });
            assert!(
                !has_save_session(&effects),
                "StreamFailed 必须不发 SaveSession (T3-3-fixA 附录 B Error 行)"
            );
        }

        /// T3-3-fixA P0-2: StreamCancelled 不发 SaveSession（用户取消不写持久化）.
        ///
        /// 固化附录 B 决策表中 Cancelled 行：reduce_stream_cancelled emit
        /// 仅 [RequestRedraw]。phase_f_stream_cancelled_no_save_no_hook 已有覆盖,
        /// 本测试用 fixA 命名保留，便于按附录 B 决策点定位回归.
        #[test]
        fn t3_3_fix_a_stream_cancelled_no_save() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-cancel".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "d-cancel".to_string(),
            });
            assert!(
                !has_save_session(&effects),
                "StreamCancelled 必须不发 SaveSession (T3-3-fixA 附录 B Cancelled 行)"
            );
        }

        /// S2-A test 3: stream_failed_effect_sequence
        ///
        /// Failure 路径（timeout / context-overflow / 其他错误）：chat::mod 主循环按
        /// S2-A 改造后投递 `Action::StreamFailed { draft_id, err, retryable }`。
        /// 期望 reducer 发射 `[LogTrace(WARN), NotifyHook(Error), RequestRedraw]`。
        #[test]
        fn test_s2a_stream_failed_effect_sequence() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-failed".to_string(),
                cancel: CancellationToken::new(),
            });
            // 模拟流式过程中先收到 partial chunk
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-failed".to_string(),
                delta: "partial...".to_string(),
                version: 1,
            });

            let effects = state.reduce(Action::StreamFailed {
                draft_id: "draft-failed".to_string(),
                err: "timeout".to_string(),
                retryable: false,
            });

            // 终态清理
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "failed 后 draft 应清空"
            );
            assert!(!state.control.generating);

            // Effect 序列断言（按 reduce_stream_failed 的发射顺序）
            let has_warn_log = effects
                .iter()
                .any(|e| matches!(e, Effect::LogTrace { level, .. } if *level == tracing::Level::WARN));
            assert!(has_warn_log, "StreamFailed 必须发 LogTrace(WARN)");

            let notify_error = effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::NotifyHook {
                        event: HookEvent::Error,
                        ..
                    }
                )
            });
            assert!(notify_error, "StreamFailed 必须发 NotifyHook(Error)");
            assert!(has_request_redraw(&effects), "StreamFailed 必须发 RequestRedraw");

            // retryable=false 应穿透到 hook payload (验证字段映射)
            let retryable_in_payload = effects.iter().any(|e| {
                if let Effect::NotifyHook {
                    event: HookEvent::Error,
                    payload,
                } = e
                {
                    payload.get("retryable").and_then(serde_json::Value::as_bool) == Some(false)
                } else {
                    false
                }
            });
            assert!(retryable_in_payload, "retryable=false 必须穿透到 NotifyHook payload");
        }

        /// S2-A test 4: stream_cancelled_effect_sequence
        ///
        /// Cancel 路径（Ctrl+C / is_tool_loop_cancelled）：chat::mod 主循环按
        /// S2-A 改造后**先**判取消、**再**分类失败 — 取消必须投递
        /// `Action::StreamCancelled { draft_id }`，而非 `StreamFailed`。
        /// 期望 reducer 仅发射 `[RequestRedraw]`（不发 hook，不发 SaveSession）。
        #[test]
        fn test_s2a_stream_cancelled_effect_sequence() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "draft-cancelled".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "draft-cancelled".to_string(),
                delta: "interrupted".to_string(),
                version: 1,
            });

            let effects = state.reduce(Action::StreamCancelled {
                draft_id: "draft-cancelled".to_string(),
            });

            // 终态清理
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "cancelled 后 draft 应清空"
            );
            assert!(!state.control.generating);
            assert!(state.control.active_cancel.is_none());

            // Effect 序列：仅 RequestRedraw — 不发 NotifyHook，不发 LogTrace(WARN)，不发 SaveSession
            assert!(has_request_redraw(&effects), "StreamCancelled 必须发 RequestRedraw");
            assert!(
                !has_notify_hook(&effects),
                "StreamCancelled 不应发 NotifyHook（取消不算错误）"
            );
            assert!(
                !has_save_session(&effects),
                "StreamCancelled 不应 SaveSession（与 Failed 一致，避免误持久化中断状态）"
            );
            let has_warn_log = effects
                .iter()
                .any(|e| matches!(e, Effect::LogTrace { level, .. } if *level == tracing::Level::WARN));
            assert!(!has_warn_log, "StreamCancelled 不应发 WARN LogTrace（cancel 不算异常）");

            // 协议契约：cancel 必须在失败分类**之前**判别 — 若误把 cancel 当 failed
            // 投递为 StreamFailed，会触发上面 stream_failed_effect_sequence 测试中
            // 的 NotifyHook(Error)，与此处的 !has_notify_hook 矛盾，从而 fail。
            // 本测试通过缺席 NotifyHook(Error) 间接验证 chat::mod 主循环的
            // "先判 is_tool_loop_cancelled → 再分类 FailedWithError" 顺序契约.
        }

        /// S2-A test 5 (Codex 阻塞): tool_call_chunk_interleave_consistency
        ///
        /// 验证同一 turn 内 tool 事件（`ToolStarted` / `ToolFinished`）与
        /// `StreamChunkReceived` **交织**时，reducer 仍然按"独立轴"维持一致状态：
        /// - stream.draft.accumulated 只由 stream chunk 累积，tool 事件不污染
        /// - tool 事件只影响 ui.conversation_lines / pending_tool_cards，不动 draft
        /// - 与"先收完所有 stream chunk、再处理 tool"的等价序列输出 state 一致
        ///
        /// 这是 chat::run 主循环 tool-call loop 与 streaming 路径并行的关键不变量 —
        /// 若 reducer 在 ToolStarted/ToolFinished 路径上误清/误改 draft，会出现
        /// 用户可见的 streaming 文字"突然回退一段"的回归。
        #[test]
        fn test_s2a_tool_call_chunk_interleave_consistency() {
            // ── 场景 A：交织序列 — stream / tool / stream / tool 交错 ──
            let mut state_a = s();
            let _ = state_a.reduce(Action::TurnStarted {
                draft_id: "draft-interleave".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state_a.reduce(Action::StreamChunkReceived {
                draft_id: "draft-interleave".to_string(),
                delta: "hello ".to_string(),
                version: 1,
            });
            let _ = state_a.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                args: "{\"q\":\"openprx\"}".to_string(),
            });
            let _ = state_a.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                success: true,
                duration_ms: 42,
                result: Some("found 3 results".to_string()),
            });
            let _ = state_a.reduce(Action::StreamChunkReceived {
                draft_id: "draft-interleave".to_string(),
                delta: "world".to_string(),
                version: 2,
            });

            // ── 场景 B：等价"纯流式后置 tool"序列 ──
            let mut state_b = s();
            let _ = state_b.reduce(Action::TurnStarted {
                draft_id: "draft-interleave".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state_b.reduce(Action::StreamChunkReceived {
                draft_id: "draft-interleave".to_string(),
                delta: "hello ".to_string(),
                version: 1,
            });
            let _ = state_b.reduce(Action::StreamChunkReceived {
                draft_id: "draft-interleave".to_string(),
                delta: "world".to_string(),
                version: 2,
            });
            let _ = state_b.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                args: "{\"q\":\"openprx\"}".to_string(),
            });
            let _ = state_b.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                success: true,
                duration_ms: 42,
                result: Some("found 3 results".to_string()),
            });

            // 核心不变量：draft.accumulated 完全相同（tool 事件不污染流式文本）
            let acc_a = state_a
                .stream
                .primary_streaming_draft()
                .map(|d| d.accumulated.clone())
                .expect("test: scenario A draft must exist");
            let acc_b = state_b
                .stream
                .primary_streaming_draft()
                .map(|d| d.accumulated.clone())
                .expect("test: scenario B draft must exist");
            assert_eq!(
                acc_a, "hello world",
                "交织序列下 draft.accumulated 仅由 stream chunk 累积"
            );
            assert_eq!(
                acc_a, acc_b,
                "交织 vs 纯流式后置 tool 的 draft.accumulated 必须字节级一致"
            );

            // version 也必须相等（tool 事件不动 version）
            assert_eq!(
                state_a.stream.primary_streaming_draft().map(|d| d.version),
                state_b.stream.primary_streaming_draft().map(|d| d.version),
                "tool 事件不应推进 stream.version"
            );
            assert_eq!(state_a.stream.primary_streaming_draft().map(|d| d.version), Some(2));

            // tool 卡片在两边都已落地，且 ToolFinished 后已从 pending 移除
            assert_eq!(
                state_a.control.pending_tool_card_count(ToolTaskKey::Primary),
                state_b.control.pending_tool_card_count(ToolTaskKey::Primary),
                "两个序列 pending_tool_cards 数量必须一致"
            );
            assert!(
                state_a.control.pending_tool_card_count(ToolTaskKey::Primary) == 0,
                "ToolFinished 后 pending_tool_cards 应清空"
            );

            // control 状态一致：仍在生成中，cancel token 未变
            assert!(state_a.control.generating);
            assert!(state_b.control.generating);
            assert!(state_a.control.active_cancel.is_some());
            assert!(state_b.control.active_cancel.is_some());
        }
    }

    // ─── S2-B 集成测试 (5 个新增测试) ─────────────────────────────────────────
    //
    // 这五个测试覆盖 S2-B 把 chat 模块的会话/取消路径接入 Redux dispatch 后的契约：
    //   1. CancelRequested 真发 CancelToken effect 并清 control 状态
    //   2. ModeChanged 与 legacy chat_session.set_mode 后 state.session.mode 等值
    //   3. RecordUserTurn 单次写入不产生重复 session.turns
    //   4. HistoryCompacted 保留 system + 控制总预算
    //   5. StreamCancelled 与 S2-A 终态行为一致（cancel 与 token cancel 不互相干扰）

    #[cfg(test)]
    mod s2b {
        use super::super::*;
        use crate::chat::action::{Action, CompactReason};
        use crate::providers::ChatMessage;
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        /// S2-B-1: redux_cancel_requested_clears_control_and_emits_cancel_effect
        ///
        /// 单击 Ctrl+C 期间：reducer 必须发 `Effect::CancelToken(token)` 真触发
        /// 底层取消（替代旧手动 `token.cancel()`），同时清 generating/draft/active_cancel.
        /// 关闭了 S2-B Codex 风险中 "UI 取消了但底层仍跑" 的窗口。
        #[test]
        fn redux_cancel_requested_clears_control_and_emits_cancel_effect() {
            let mut state = s();
            let tok = CancellationToken::new();
            // 开 turn → control.active_cancel=Some(tok), generating=true
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-s2b-1".to_string(),
                cancel: tok,
            });
            assert!(state.control.generating);
            assert!(state.control.active_cancel.is_some());

            let effects = state.reduce(Action::CancelRequested);

            // control 状态必须清干净
            assert!(!state.control.generating, "CancelRequested 后 generating=false");
            assert!(
                state.stream.primary_streaming_draft().is_none(),
                "CancelRequested 后 draft 清空"
            );
            assert!(
                state.control.active_cancel.is_none(),
                "CancelRequested 后 active_cancel 清空（token 已交给 Effect::CancelToken）"
            );

            // Effect 序列必须含 CancelToken（关键 — 真取消）+ CancelDraft + LogTrace + RequestRedraw
            let has_cancel_token = effects.iter().any(|e| matches!(e, Effect::CancelToken(_)));
            assert!(
                has_cancel_token,
                "CancelRequested 必须发 Effect::CancelToken — 这是 S2-B 关键差异"
            );
            let has_cancel_draft = effects
                .iter()
                .any(|e| matches!(e, Effect::CancelDraft(id) if id == "d-s2b-1"));
            assert!(has_cancel_draft, "应含 CancelDraft(draft-id)");
            // CancelToken 必须在 CancelDraft 之前（先真取消底层，再清 UI）
            let pos_token = effects
                .iter()
                .position(|e| matches!(e, Effect::CancelToken(_)))
                .expect("CancelToken present");
            let pos_draft = effects
                .iter()
                .position(|e| matches!(e, Effect::CancelDraft(_)))
                .expect("CancelDraft present");
            assert!(pos_token < pos_draft, "CancelToken 必须在 CancelDraft 之前");
        }

        /// S2-B-2: redux_mode_changed_matches_legacy_chat_session_mode
        ///
        /// 双写期 reducer `state.session.mode` 必须与 legacy `chat_session.mode` 同步.
        /// 此测试通过对 ChatSession 和 ChatState 都跑相同序列（set_mode + ModeChanged）
        /// 验证 mode 最终值一致。
        #[test]
        fn redux_mode_changed_matches_legacy_chat_session_mode() {
            use crate::chat::session::ChatSession;
            let mut state = s();
            let mut legacy = ChatSession::new("openai", "gpt-4o-mini");

            // Plan
            let _ = state.reduce(Action::ModeChanged(ChatMode::Plan));
            legacy.set_mode(ChatMode::Plan);
            assert_eq!(state.session.mode, legacy.mode, "Plan 模式应一致");
            assert_eq!(state.ui.chat_mode, ChatMode::Plan, "Plan 模式应进入 UI status");

            // Auto
            let _ = state.reduce(Action::ModeChanged(ChatMode::Auto));
            legacy.set_mode(ChatMode::Auto);
            assert_eq!(state.session.mode, legacy.mode, "Auto 模式应一致");
            assert_eq!(state.ui.chat_mode, ChatMode::Auto, "Auto 模式应进入 UI status");

            // Edit (default)
            let _ = state.reduce(Action::ModeChanged(ChatMode::Edit));
            legacy.set_mode(ChatMode::Edit);
            assert_eq!(state.session.mode, legacy.mode, "Edit 模式应一致");
            assert_eq!(state.ui.chat_mode, ChatMode::Edit, "Edit 模式应进入 UI status");
        }

        #[test]
        fn p8_mode_changed_does_not_escalate_autonomy_or_policy() {
            use crate::approval::ApprovalManager;
            use crate::config::AutonomyConfig;
            use crate::security::policy::ToolDecision;
            use crate::security::{AutonomyLevel, SecurityPolicy};

            let mut state = s();
            let mut autonomy = AutonomyConfig {
                level: AutonomyLevel::ReadOnly,
                ..AutonomyConfig::default()
            };
            autonomy.sandbox.enabled = Some(false);
            let autonomy_before = autonomy.clone();
            let policy_before = SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));
            let approval_before = ApprovalManager::from_config(&autonomy);
            state.ui.autonomy_level = autonomy.level;

            for mode in [ChatMode::Plan, ChatMode::Edit, ChatMode::Auto, ChatMode::Plan] {
                let _ = state.reduce(Action::ModeChanged(mode));
                assert_eq!(state.ui.autonomy_level, AutonomyLevel::ReadOnly);
            }

            let policy_after = SecurityPolicy::from_config(&autonomy, std::path::Path::new("/tmp"));
            let approval_after = ApprovalManager::from_config(&autonomy);

            assert_eq!(autonomy.level, autonomy_before.level);
            assert_eq!(autonomy.workspace_only, autonomy_before.workspace_only);
            assert_eq!(autonomy.sandbox.enabled, autonomy_before.sandbox.enabled);
            assert_eq!(policy_after.autonomy, policy_before.autonomy);
            assert_eq!(approval_after.autonomy_level(), approval_before.autonomy_level());
            assert_eq!(state.session.mode, ChatMode::Plan);
            assert_eq!(
                policy_after.decide("file_write", "user", "terminal", "chat"),
                ToolDecision::Deny,
                "ChatMode::Auto cannot widen read_only autonomy because decide() is ChatMode-free"
            );
        }

        /// BUG-07: `ModelChanged` reducer 更新 `session.model`，使 status bar 立刻
        /// 反映新 model，且新值进入 UI snapshot（snapshot.model 取 session.model）。
        #[test]
        fn redux_model_changed_updates_session_and_snapshot() {
            let mut state = s();
            assert_eq!(&*state.session.model, "gpt-4o-mini", "初始 model");

            let effects = state.reduce(Action::ModelChanged {
                model: "anthropic/claude-sonnet-4".to_string(),
            });
            assert_eq!(&*state.session.model, "anthropic/claude-sonnet-4", "model 已切换");
            assert!(
                effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
                "ModelChanged 应请求重绘以刷新 status bar"
            );

            // snapshot.model 取自 session.model；build_ui_snapshot 仅在 terminal-tui
            // feature 下存在，故 snapshot 断言对该 feature 收口。
            #[cfg(feature = "terminal-tui")]
            {
                let snap = state.build_ui_snapshot(1);
                assert_eq!(&*snap.model, "anthropic/claude-sonnet-4", "snapshot.model 反映新 model");
            }
        }

        /// Bug #3: `ProviderChanged` reducer 更新 `session.provider`，使 status bar
        /// `state.provider()`（取自 snapshot.provider ← session.provider）立刻反映新
        /// provider。`model: None` 时不动 session.model。
        #[test]
        fn redux_provider_changed_updates_session_provider_only() {
            let mut state = s();
            assert_eq!(&*state.session.provider, "openai", "初始 provider");
            assert_eq!(&*state.session.model, "gpt-4o-mini", "初始 model");

            let effects = state.reduce(Action::ProviderChanged {
                provider: "openrouter".to_string(),
                model: None,
            });
            assert_eq!(&*state.session.provider, "openrouter", "provider 已切换");
            assert_eq!(&*state.session.model, "gpt-4o-mini", "model: None 时 model 不变");
            assert!(
                effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
                "ProviderChanged 应请求重绘以刷新 status bar"
            );

            #[cfg(feature = "terminal-tui")]
            {
                let snap = state.build_ui_snapshot(1);
                assert_eq!(&*snap.provider, "openrouter", "snapshot.provider 反映新 provider");
            }
        }

        /// Bug #3: `ProviderChanged` 携带 `model: Some(..)` 时同时同步 session.model
        /// （切 provider 时显式带了兼容 model 参数的情形）。
        #[test]
        fn redux_provider_changed_with_model_updates_both() {
            let mut state = s();
            let effects = state.reduce(Action::ProviderChanged {
                provider: "anthropic".to_string(),
                model: Some("claude-sonnet-4".to_string()),
            });
            assert_eq!(&*state.session.provider, "anthropic", "provider 已切换");
            assert_eq!(&*state.session.model, "claude-sonnet-4", "model 一并切换");
            assert!(effects.iter().any(|e| matches!(e, Effect::RequestRedraw)));

            #[cfg(feature = "terminal-tui")]
            {
                let snap = state.build_ui_snapshot(1);
                assert_eq!(&*snap.provider, "anthropic");
                assert_eq!(&*snap.model, "claude-sonnet-4");
            }
        }

        /// T3-3-d-byte-parity: reducer `session.history` 与 legacy `ChatSession.turns`
        /// 在 Both 模式下应字节级对账（同一条 user/assistant 内容写入两端时内容一致）.
        ///
        /// 这把 Both 模式"双写期对账"做成 in-process 单测，避免 PTY 比对的 noise.
        /// 关键断言：
        ///   1. session.turns.len() == legacy.turns.len()（条目数对齐）
        ///   2. role 序列完全一致
        ///   3. content 字节级一致（无 sanitization 差异时）
        #[test]
        fn t3_3d_both_mode_history_byte_level_parity() {
            use crate::chat::session::ChatSession;
            let mut state = s();
            let mut legacy = ChatSession::new("test-prov", "test-model");

            let inputs = [
                ("user", "hi there"),
                ("assistant", "hello!"),
                ("user", "explain monads in 1 sentence"),
                ("assistant", "a monad is a monoid in the category of endofunctors"),
                ("user", ""), // 空字符串边界
                ("assistant", "🚀 unicode 中文 mixed content"),
            ];

            for (role, text) in inputs {
                if role == "user" {
                    let _ = state.reduce(Action::RecordUserTurn(text.to_string()));
                    legacy.add_user_turn(text);
                } else {
                    // 测试输入闭包所有 role 都是 "user" / "assistant"，else 分支即 assistant
                    let _ = state.reduce(Action::RecordAssistantTurn {
                        task_id: None,
                        content: text.to_string(),
                    });
                    legacy.add_assistant_turn(text, Vec::new());
                }
            }

            assert_eq!(
                state.session.turns.len(),
                legacy.turns.len(),
                "Both 模式：reducer.session.turns.len() 应等于 legacy.turns.len()"
            );
            for (i, (lhs, rhs)) in state.session.turns.iter().zip(legacy.turns.iter()).enumerate() {
                assert_eq!(lhs.role, rhs.role, "turn {i} role 不一致");
                assert_eq!(
                    lhs.content.as_bytes(),
                    rhs.content.as_bytes(),
                    "turn {i} content 字节级不一致（reducer vs legacy）"
                );
            }
            // history 与 turns 同步增长
            assert_eq!(
                state.session.history.len(),
                state.session.turns.len(),
                "reducer.session.history.len() 应与 turns.len() 同步"
            );
        }

        /// T3-3-fixB C1: reducer `session.history` 在 system 维度与 legacy 手动 history
        /// 字节级 parity. t3_3d_both_mode_history_byte_level_parity 只覆盖 user/assistant,
        /// 这里补 SetLeadingSystemPrompt (upsert) + RecordSystemMessage (append) 维度.
        ///
        /// legacy 侧用 `Vec<ChatMessage>` 手动镜像（ChatSession 没有专用 add_system_turn,
        /// chat::run 主循环直接操作 history 切片），保持 reducer 与 chat::run 字节级对齐.
        ///
        /// 注：tool_calls parity gap 仍在（reducer RecordAssistantTurn 忽略 tool_calls
        /// 参数，session.turns[i].tool_calls 一律 Vec::new()），挂 S2.5 横切统一处理.
        #[test]
        fn t3_3_fix_b_both_parity_system_history() {
            use crate::providers::ChatMessage;
            let mut state = s();
            let mut legacy: Vec<ChatMessage> = Vec::new();

            // 1) SetLeadingSystemPrompt 空 history → push system v1
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "rules v1".to_string(),
            });
            legacy.push(ChatMessage::system("rules v1"));

            // 2) RecordUserTurn → append user
            let _ = state.reduce(Action::RecordUserTurn("u1".to_string()));
            legacy.push(ChatMessage::user("u1"));

            // 3) SetLeadingSystemPrompt 非空 history → 替换 history[0]
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "rules v2".to_string(),
            });
            if let Some(first) = legacy.first_mut() {
                *first = ChatMessage::system("rules v2");
            }

            // 4) RecordAssistantTurn → append assistant
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "a1".to_string(),
            });
            legacy.push(ChatMessage::assistant("a1"));

            // 5) RecordSystemMessage → append system 到末尾（/clear 后场景）
            let _ = state.reduce(Action::RecordSystemMessage {
                content: "context note".to_string(),
            });
            legacy.push(ChatMessage::system("context note"));

            // ── 字节级 parity ──
            assert_eq!(
                state.session.history.len(),
                legacy.len(),
                "history.len() 与 legacy 手动镜像应一致"
            );
            for (i, (lhs, rhs)) in state.session.history.iter().zip(legacy.iter()).enumerate() {
                assert_eq!(lhs.role, rhs.role, "history[{i}] role 不一致");
                assert_eq!(
                    lhs.content.as_bytes(),
                    rhs.content.as_bytes(),
                    "history[{i}] content 字节级不一致"
                );
            }
        }

        /// S2-B-3: redux_record_turns_single_write_no_duplicate_session_turns
        ///
        /// dispatch `RecordUserTurn` + `RecordAssistantTurn` 各一次后，
        /// `state.session.turns` 必须恰好增长 +2，绝不产生重复条目（之前的 1197+2055
        /// 双 dispatch 已合并为 enriched 同点一次 dispatch）。
        #[test]
        fn redux_record_turns_single_write_no_duplicate_session_turns() {
            let mut state = s();
            assert_eq!(state.session.turns.len(), 0);

            let _ = state.reduce(Action::RecordUserTurn("hello".to_string()));
            assert_eq!(state.session.turns.len(), 1);
            assert_eq!(state.session.history.len(), 1, "history 也应同步增长（reducer 单写）");

            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "hi back".to_string(),
            });
            assert_eq!(state.session.turns.len(), 2, "user + assistant 两条，无重复");
            assert_eq!(state.session.history.len(), 2, "history 也应是 user+assistant 两条");

            // 关键防回归：用同样的内容再 dispatch 一次，turns 应增长为 4，不是被去重为 2
            // （reducer 不做幂等性—去重由调用方保证；此测试确认 reducer 是 append-only）
            let _ = state.reduce(Action::RecordUserTurn("hello".to_string()));
            assert_eq!(state.session.turns.len(), 3, "再次 dispatch 必须 append 一条");
        }

        /// S2-B-4: redux_compaction_action_preserves_system_and_budget
        ///
        /// HistoryCompacted 必须保留 system prompt 且总字符数 ≤ COMPACT_TOTAL_CHARS.
        /// 这是 chat::mod 主循环 context-overflow 重试路径的核心契约。
        #[test]
        fn redux_compaction_action_preserves_system_and_budget() {
            let mut state = s();
            // system + 20 条长 user/assistant 消息
            state
                .session
                .history
                .push(ChatMessage::system("system rules — must survive compaction"));
            for i in 0..20 {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                state.session.history.push(ChatMessage {
                    role: role.to_string(),
                    content: format!("turn-{i} {}", "y".repeat(400)),
                });
            }

            let effects = state.reduce(Action::HistoryCompacted {
                reason: CompactReason::ContextOverflow,
            });

            // System prompt 必须保留在首位
            assert_eq!(
                state.session.history.first().map(|m| m.role.as_str()),
                Some("system"),
                "compaction 后 system 仍在首位"
            );
            assert!(
                state
                    .session
                    .history
                    .first()
                    .is_some_and(|m| m.content.contains("must survive")),
                "system 内容必须完整保留（不被截断）"
            );
            // 非 system 部分总预算 ≤ COMPACT_TOTAL_CHARS
            let non_system_chars: usize = state
                .session
                .history
                .iter()
                .skip(1)
                .map(|m| m.content.chars().count())
                .sum();
            assert!(
                non_system_chars <= super::COMPACT_TOTAL_CHARS,
                "非 system 总字符 {non_system_chars} 必须 ≤ {}",
                super::COMPACT_TOTAL_CHARS
            );
            // 至少发 LogTrace
            assert!(
                effects.iter().any(|e| matches!(e, Effect::LogTrace { .. })),
                "HistoryCompacted 必须发 LogTrace"
            );
        }

        /// S2-B-5: redux_stream_cancelled_cooperates_with_s2a_terminal_actions
        ///
        /// 用户在流式期间 Ctrl+C → reducer 发 CancelToken 取消底层 + 清状态.
        /// 紧接着 chat::run 主循环投递 `StreamCancelled` 作为 turn 终态 — reducer
        /// 应是 no-op（draft 已清），不重复发 hook 也不打错 effect 顺序。
        #[test]
        fn redux_stream_cancelled_cooperates_with_s2a_terminal_actions() {
            let mut state = s();
            let tok = CancellationToken::new();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-coop".to_string(),
                cancel: tok.clone(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d-coop".to_string(),
                delta: "partial".to_string(),
                version: 1,
            });

            // 1. 用户 Ctrl+C → CancelRequested
            let cancel_effects = state.reduce(Action::CancelRequested);
            // 真取消 token (通过 effect 验证 — reducer 已 take 出来)
            let cancel_token_effect = cancel_effects.iter().find_map(|e| match e {
                Effect::CancelToken(t) => Some(t.clone()),
                _ => None,
            });
            let token_from_effect = cancel_token_effect.expect("CancelToken effect 必须存在");
            // EffectExecutor 真调 cancel — 此处模拟
            token_from_effect.cancel();
            assert!(
                tok.is_cancelled(),
                "原 token 应被 effect 中的 token 取消（共享 cancellation）"
            );
            // control 已清
            assert!(!state.control.generating);
            assert!(state.stream.primary_streaming_draft().is_none());

            // 2. chat::run 主循环检测到 cancellation → 投递 StreamCancelled 终态
            let terminal_effects = state.reduce(Action::StreamCancelled {
                draft_id: "d-coop".to_string(),
            });
            // 此时 draft 已被 CancelRequested 清，StreamCancelled 必须是 no-op（不重发 hook）
            let has_notify = terminal_effects.iter().any(|e| matches!(e, Effect::NotifyHook { .. }));
            assert!(
                !has_notify,
                "StreamCancelled (draft 已清) 不应再发 NotifyHook — 避免双发"
            );
            // 也不应再有 CancelToken（token 已经发过且取消）
            let has_cancel_token = terminal_effects.iter().any(|e| matches!(e, Effect::CancelToken(_)));
            assert!(!has_cancel_token, "StreamCancelled 不应再发 CancelToken");
        }

        /// S2-B-6 (Codex 阻塞): cancel_shutdown_race_single_terminal
        ///
        /// 用户在流式 turn 内**几乎同时**触发 `CancelRequested` + `ShutdownRequested`
        /// （典型：长按 Ctrl+C 后立刻 Ctrl+D / SIGTERM）时:
        /// - 第一发 CancelRequested take 走 active_cancel → 发 `Effect::CancelToken`
        /// - 第二发 ShutdownRequested 看到 `generating == false` → **不应**再发
        ///   `Effect::CancelToken`（otherwise 会 take 一个已被 take 走的 Option，
        ///   或更糟，发个 None token 让 EffectExecutor 解引用）
        ///
        /// 验证两点契约:
        /// 1. 整个序列只发 **一个** `Effect::CancelToken`（terminal cancel 是单次的）
        /// 2. 第二发 ShutdownRequested 不会 panic / 不会重复 cancel / 仍发 `Effect::Quit`
        #[test]
        fn test_s2b_cancel_shutdown_race_single_terminal() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-race".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamChunkReceived {
                draft_id: "d-race".to_string(),
                delta: "streaming...".to_string(),
                version: 1,
            });
            assert!(state.control.generating);
            assert!(state.control.active_cancel.is_some());

            // 1. CancelRequested — take active_cancel + 发 CancelToken
            let cancel_effects = state.reduce(Action::CancelRequested);
            let cancel_token_count = cancel_effects
                .iter()
                .filter(|e| matches!(e, Effect::CancelToken(_)))
                .count();
            assert_eq!(cancel_token_count, 1, "CancelRequested 阶段应发恰好 1 个 CancelToken");
            assert!(!state.control.generating, "CancelRequested 后 generating=false");
            assert!(state.control.active_cancel.is_none(), "active_cancel 已被 take 走");

            // 2. ShutdownRequested — 此时 generating=false, active_cancel=None.
            //    reducer 必须**不**再发 CancelToken（避免双重 cancel + 防御性 take None）
            let shutdown_effects = state.reduce(Action::ShutdownRequested);
            let shutdown_cancel_token_count = shutdown_effects
                .iter()
                .filter(|e| matches!(e, Effect::CancelToken(_)))
                .count();
            assert_eq!(
                shutdown_cancel_token_count, 0,
                "ShutdownRequested 在 active_cancel 已被 take 后不应再发 CancelToken"
            );
            // 同样不应再发 CancelDraft（draft 已被 CancelRequested 清）
            let shutdown_cancel_draft_count = shutdown_effects
                .iter()
                .filter(|e| matches!(e, Effect::CancelDraft(_)))
                .count();
            assert_eq!(
                shutdown_cancel_draft_count, 0,
                "ShutdownRequested 在 draft 已清后不应再发 CancelDraft"
            );
            // 但必须发 Effect::Quit
            assert!(
                shutdown_effects.iter().any(|e| matches!(e, Effect::Quit)),
                "ShutdownRequested 必须发 Effect::Quit"
            );

            // 全序列只发 1 个 CancelToken（terminal effect 唯一）
            let total_cancel_tokens = cancel_effects
                .iter()
                .chain(shutdown_effects.iter())
                .filter(|e| matches!(e, Effect::CancelToken(_)))
                .count();
            assert_eq!(
                total_cancel_tokens, 1,
                "整个 race 序列只能发 1 个 Effect::CancelToken（terminal cancel 单一性）"
            );

            // 终态：generating=false, active_cancel=None, draft=None — 不留残留
            assert!(!state.control.generating);
            assert!(state.control.active_cancel.is_none());
            assert!(state.stream.primary_streaming_draft().is_none());

            // 反向 race 验证：构造另一份 state，先 Shutdown 再 Cancel —
            // 同样只能发 1 个 CancelToken（首发 take 走 token，后续 Cancel 在
            // generating=false 下 no-op）.
            let mut state2 = s();
            let tok2 = CancellationToken::new();
            let _ = state2.reduce(Action::TurnStarted {
                draft_id: "d-race-2".to_string(),
                cancel: tok2,
            });
            let first_effects = state2.reduce(Action::ShutdownRequested);
            let second_effects = state2.reduce(Action::CancelRequested);
            let cancel_token_total = first_effects
                .iter()
                .chain(second_effects.iter())
                .filter(|e| matches!(e, Effect::CancelToken(_)))
                .count();
            assert_eq!(
                cancel_token_total, 1,
                "反向 race（Shutdown→Cancel）同样只发 1 个 CancelToken"
            );
            // 第二发 CancelRequested 必须是 no-op（generating=false）
            assert!(
                second_effects.is_empty(),
                "Shutdown 后 generating=false，CancelRequested 必须 no-op，实际: {second_effects:?}"
            );
        }
    }

    // ─── S2-C 集成测试 (3 个新增测试) ─────────────────────────────────────────
    //
    // 这三个测试覆盖 S2-C 把 chat 模块的 mirror / history 路径接入 Redux dispatch 后
    // 的契约：
    //   1. /clear 路径 reducer 端保留 system + UI 镜像有 system message 行
    //   2. user/assistant 双 dispatch 后 session.turns + session.history 顺序稳定
    //   3. SystemMessageAdded 与 RecordSystemMessage 不串扰（UI 与 history 是两个轴）
    //
    // 关键设计决策（来自 Codex P0 审计）:
    //   - 不引入 legacy_mirror_enabled / legacy_history_enabled 守卫——legacy
    //     history 仍是 LLM 真上下文源，reducer 是观察账本。S2-C 只新增 dispatch.
    //   - SetLeadingSystemPrompt 区别于 RecordSystemMessage：前者 upsert 首位（每轮
    //     turn 都跑，覆盖 skill 列表变化），后者 append（/clear 后重建）.
    #[cfg(test)]
    mod s2c {
        use super::super::*;
        use crate::chat::action::Action;
        use crate::providers::ChatMessage;
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        /// S2-C-1: redux_history_cleared_on_slash_clear_keeps_system_only
        ///
        /// 模拟 /clear 路径：reducer 收到 HistoryCleared 时必须保留所有 system
        /// 消息、清空 user/assistant。验证 reducer 与 legacy `history.clear() +
        /// 条件 push system` 终态等价（仅当 skill_rag 关闭时 legacy 重 push；
        /// reducer 直接保留已有 system，无需重 push 等价于 skill_rag.enabled 路径）。
        #[test]
        fn redux_history_cleared_on_slash_clear_keeps_system_only() {
            let mut state = s();
            // 起始 history: system + 2 user + 2 assistant
            state.session.history.push(ChatMessage::system("sys-prompt-v1"));
            state.session.history.push(ChatMessage::user("u1"));
            state.session.history.push(ChatMessage::assistant("a1"));
            state.session.history.push(ChatMessage::user("u2"));
            state.session.history.push(ChatMessage::assistant("a2"));
            assert_eq!(state.session.history.len(), 5);

            let effects = state.reduce(Action::HistoryCleared);

            // 终态：只保留 system 那一条
            assert_eq!(
                state.session.history.len(),
                1,
                "HistoryCleared 后 history 应只剩 1 条 system"
            );
            let kept = state.session.history.first().expect("test: history 应有 1 条 system");
            assert_eq!(kept.role, "system");
            assert_eq!(kept.content, "sys-prompt-v1");

            // 必发 RequestRedraw + LogTrace
            assert!(effects.iter().any(|e| matches!(e, Effect::RequestRedraw)));
            assert!(effects.iter().any(|e| matches!(e, Effect::LogTrace { .. })));

            // 边界：连续 /clear 应幂等（system 仍只一条）
            let _ = state.reduce(Action::HistoryCleared);
            assert_eq!(state.session.history.len(), 1, "二次 /clear 仍保留 1 条 system");
        }

        #[cfg(feature = "terminal-tui")]
        #[test]
        fn redux_history_cleared_with_notice_keeps_visible_feedback() {
            let mut state = s();
            state.session.history.push(ChatMessage::system("sys-prompt-v1"));
            state.ui.conversation_lines.push(ConversationLine::User {
                content: "/clear".to_string(),
            });

            let effects = state.reduce(Action::HistoryClearedWithNotice {
                notice: "Conversation cleared (kept system prompt).".to_string(),
            });

            assert_eq!(state.session.history.len(), 1);
            assert_eq!(state.ui.conversation_lines.len(), 1);
            assert!(
                matches!(state.ui.conversation_lines.first(), Some(ConversationLine::System { content }) if content.contains("Conversation cleared")),
                "clear notice must survive the same reducer step that clears conversation_lines"
            );
            assert!(effects.iter().any(|e| matches!(e, Effect::RequestRedraw)));
        }

        /// S2-C-2: redux_history_append_order_user_assistant_stable
        ///
        /// dispatch SetLeadingSystemPrompt + RecordUserTurn + RecordAssistantTurn 后,
        /// session.history 顺序必须稳定为 [system, user, assistant]，且与同序 legacy
        /// `history.push` 序列字节级一致。验证 reducer 不会重排 / 不会漏 push.
        #[test]
        fn redux_history_append_order_user_assistant_stable() {
            let mut state = s();
            assert!(state.session.history.is_empty());

            // SetLeadingSystemPrompt 在空 history 上应 push（与 legacy
            // `if history.is_empty() { push }` 等价）
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "system-rules".to_string(),
            });
            assert_eq!(state.session.history.len(), 1);
            let h0 = state.session.history.first().expect("test: history[0] after push");
            assert_eq!(h0.role, "system");

            // 再次 SetLeadingSystemPrompt（typical: 每轮 turn 都跑）应替换首位，不 append
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "system-rules-v2".to_string(),
            });
            assert_eq!(
                state.session.history.len(),
                1,
                "SetLeadingSystemPrompt 二次调用必须 upsert 首位，不能 append"
            );
            let h0v2 = state.session.history.first().expect("test: history[0] after upsert");
            assert_eq!(h0v2.content, "system-rules-v2");

            // RecordUserTurn → append user
            let _ = state.reduce(Action::RecordUserTurn("user-q1".to_string()));
            assert_eq!(state.session.history.len(), 2);
            let h1 = state.session.history.get(1).expect("test: history[1] = user");
            assert_eq!(h1.role, "user");
            assert_eq!(h1.content, "user-q1");
            assert_eq!(state.session.turns.len(), 1, "session.turns 也应增长（user）");

            // RecordAssistantTurn → append assistant
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "assistant-r1".to_string(),
            });
            assert_eq!(state.session.history.len(), 3);
            let h2 = state.session.history.get(2).expect("test: history[2] = assistant");
            assert_eq!(h2.role, "assistant");
            assert_eq!(h2.content, "assistant-r1");
            assert_eq!(state.session.turns.len(), 2, "session.turns +1（assistant）");

            // 再来一轮 — 顺序应仍稳定 system, user, assistant, user, assistant
            let _ = state.reduce(Action::RecordUserTurn("user-q2".to_string()));
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "assistant-r2".to_string(),
            });
            assert_eq!(state.session.history.len(), 5);
            let roles: Vec<&str> = state.session.history.iter().map(|m| m.role.as_str()).collect();
            assert_eq!(
                roles,
                vec!["system", "user", "assistant", "user", "assistant"],
                "顺序必须稳定"
            );
            // session.turns 不含 system（仅 user/assistant 是回合）
            assert_eq!(state.session.turns.len(), 4);
            let turn_roles: Vec<&str> = state.session.turns.iter().map(|t| t.role.as_str()).collect();
            assert_eq!(turn_roles, vec!["user", "assistant", "user", "assistant"]);
        }

        /// S2-C-3: redux_system_message_mirror_and_state_consistent
        ///
        /// SystemMessageAdded 应只动 UI 镜像（ui.conversation_lines），不污染
        /// session.history；RecordSystemMessage 应只动 session.history，不污染
        /// ui.conversation_lines. 两条路径正交。
        #[cfg(feature = "terminal-tui")]
        #[test]
        fn redux_system_message_mirror_and_state_consistent() {
            use crate::chat::tui::ConversationLine;
            let mut state = s();
            assert_eq!(state.ui.conversation_lines.len(), 0);
            assert_eq!(state.session.history.len(), 0);

            // SystemMessageAdded → 只动 UI mirror
            let effects = state.reduce(Action::SystemMessageAdded {
                text: "Banner v1".to_string(),
            });
            assert_eq!(state.ui.conversation_lines.len(), 1, "ui mirror 应增长");
            let first_line = state
                .ui
                .conversation_lines
                .first()
                .expect("test: conversation_lines[0] after SystemMessageAdded");
            assert!(
                matches!(
                    first_line,
                    ConversationLine::System { content } if content == "Banner v1"
                ),
                "应是 ConversationLine::System variant"
            );
            assert_eq!(state.session.history.len(), 0, "session.history 必须不动");
            assert!(effects.iter().any(|e| matches!(e, Effect::RequestRedraw)));

            // RecordSystemMessage → 只动 session.history
            let _ = state.reduce(Action::RecordSystemMessage {
                content: "ctx-system-1".to_string(),
            });
            assert_eq!(state.session.history.len(), 1, "session.history 应增长");
            let first_hist = state.session.history.first().expect("test: history[0] = system");
            assert_eq!(first_hist.role, "system");
            assert_eq!(first_hist.content, "ctx-system-1");
            assert_eq!(
                state.ui.conversation_lines.len(),
                1,
                "ui mirror 必须不动（仍是 1 条 banner）"
            );

            // 多发几条 SystemMessageAdded — UI mirror 单调增长，history 仍不变
            let _ = state.reduce(Action::SystemMessageAdded {
                text: "Slash output 1".to_string(),
            });
            let _ = state.reduce(Action::SystemMessageAdded {
                text: "Slash output 2".to_string(),
            });
            assert_eq!(state.ui.conversation_lines.len(), 3);
            assert_eq!(state.session.history.len(), 1, "session.history 仍是 1");
        }

        /// S2-C-bonus2 (Codex P0 回归): /clear 后再 dispatch SetLeadingSystemPrompt
        /// 应保持终态 ≤ 1 条 system —— 若误用 RecordSystemMessage 会累计成 2+ 条
        /// system. 本测试模拟 mod.rs:1254-1287 /clear !skill_rag.enabled 完整路径.
        #[test]
        fn redux_clear_then_set_leading_yields_single_system() {
            let mut state = s();
            // 起始 history: system + 几条会话
            state.session.history.push(ChatMessage::system("old-system"));
            state.session.history.push(ChatMessage::user("u1"));
            state.session.history.push(ChatMessage::assistant("a1"));
            state.session.history.push(ChatMessage::user("u2"));
            assert_eq!(state.session.history.len(), 4);

            // Step 1: HistoryCleared (与 legacy `history.clear()` 双写) —
            // reducer 保留旧 system，drain user/assistant
            let _ = state.reduce(Action::HistoryCleared);
            assert_eq!(state.session.history.len(), 1, "HistoryCleared 后仅保留 1 条旧 system");
            let after_clear = state.session.history.first().expect("test: post-clear history[0]");
            assert_eq!(after_clear.content, "old-system");

            // Step 2: SetLeadingSystemPrompt (与 legacy `history.push(new system)` 双写) —
            // upsert: 替换已有首位 system 为新 prompt（绝不 append）
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "new-system".to_string(),
            });
            // 关键：终态必须仍是 1 条 system，content 是新的（不是 2 条 system）
            assert_eq!(
                state.session.history.len(),
                1,
                "/clear + SetLeadingSystemPrompt 终态必须 1 条 system，不能累计"
            );
            let after_reset = state.session.history.first().expect("test: post-reset history[0]");
            assert_eq!(after_reset.role, "system");
            assert_eq!(after_reset.content, "new-system");

            // 防回归：若误用 RecordSystemMessage 会变成 2 条 — 这里显式验证
            // SetLeadingSystemPrompt 不是 append 语义
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "newer-system".to_string(),
            });
            assert_eq!(
                state.session.history.len(),
                1,
                "再次 SetLeadingSystemPrompt 仍 upsert，不 append"
            );
        }

        /// S2-C-bonus (Codex 建议): SetLeadingSystemPrompt 在非空 history 上必须
        /// 替换首位，不能 append — 防回归 1336 语义。
        #[test]
        fn set_leading_system_prompt_replaces_first_instead_of_append() {
            let mut state = s();
            // 先预置 history: [system-old, user1, assistant1]
            state.session.history.push(ChatMessage::system("system-old"));
            state.session.history.push(ChatMessage::user("user1"));
            state.session.history.push(ChatMessage::assistant("assistant1"));
            assert_eq!(state.session.history.len(), 3);

            // SetLeadingSystemPrompt 必须替换首位 system，不能 append
            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "system-new".to_string(),
            });
            assert_eq!(
                state.session.history.len(),
                3,
                "SetLeadingSystemPrompt 不能改变 history 长度（替换不 append）"
            );
            let h0 = state.session.history.first().expect("test: history[0] = system-new");
            assert_eq!(h0.role, "system");
            assert_eq!(h0.content, "system-new");
            // user / assistant 顺序不变
            let h1 = state.session.history.get(1).expect("test: history[1] = user1");
            assert_eq!(h1.role, "user");
            assert_eq!(h1.content, "user1");
            let h2 = state.session.history.get(2).expect("test: history[2] = assistant1");
            assert_eq!(h2.role, "assistant");
        }

        #[test]
        fn set_leading_system_prompt_preserves_resumed_history_without_system() {
            let mut state = s();
            state.session.history.push(ChatMessage::user("resumed-user"));
            state.session.history.push(ChatMessage::assistant("resumed-assistant"));

            let _ = state.reduce(Action::SetLeadingSystemPrompt {
                content: "system-new".to_string(),
            });

            assert_eq!(state.session.history.len(), 3);
            let h0 = state.session.history.first().expect("test: inserted system prompt");
            assert_eq!(h0.role, "system");
            assert_eq!(h0.content, "system-new");
            let h1 = state.session.history.get(1).expect("test: resumed user preserved");
            assert_eq!(h1.role, "user");
            assert_eq!(h1.content, "resumed-user");
            let h2 = state.session.history.get(2).expect("test: resumed assistant preserved");
            assert_eq!(h2.role, "assistant");
            assert_eq!(h2.content, "resumed-assistant");
        }
    }

    // ─── S2.5 P1-B: tool_calls parity via reducer 内回填 ─────────────────────
    //
    // 方案 C 用 ControlState.current_turn_tool_calls 在 reducer 内缓冲：
    //   ToolStarted/Finished 累积，RecordAssistantTurn 用 mem::take 回填到
    //   session.turns.last_mut().tool_calls，stream 终态 + InputSubmitted 兜底清空.
    // 关闭 state.rs:1171 原 FIXME(S2.5)，零 Action 签名变更，零 callsite 破坏.
    #[cfg(test)]
    mod p1_b_tool_calls_parity {
        use super::super::*;
        use crate::chat::action::Action;
        use crate::chat::session::ToolCallSummary;
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        /// S2.5 P1-B: RecordAssistantTurn 回填 tool_calls 到 session.turns.last_mut().tool_calls.
        #[test]
        fn s2_5_p1_b_assistant_turn_carries_tool_calls() {
            let mut state = s();
            let _ = state.reduce(Action::RecordUserTurn("question".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-p1b-1".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                success: true,
                duration_ms: 12,
                result: Some("ok".to_string()),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "answer".to_string(),
            });

            let last = state.session.turns.last().expect("test: assistant turn");
            assert_eq!(last.role, "assistant");
            assert_eq!(last.tool_calls.len(), 1, "本轮 1 个 tool_call 必须回填");
            let call: &ToolCallSummary = last.tool_calls.first().expect("test: tool_calls[0]");
            assert_eq!(call.name, "shell");
            assert!(call.success);
            assert_eq!(call.args_preview, r#"command="ls""#);

            // 回填后 ControlState 缓冲必须清空（mem::take + clear）.
            assert!(!state.control.tool_buffers.contains_key(&ToolTaskKey::Primary));
        }

        /// S2.5 P1-B: 多个 tool 在同一 turn 内按顺序聚合.
        #[test]
        fn s2_5_p1_b_multi_tool_aggregates_in_turn() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-p1b-2".to_string(),
                cancel: CancellationToken::new(),
            });
            for (i, ok) in [(1u8, true), (2, false), (3, true)] {
                let name = format!("tool{i}");
                let _ = state.reduce(Action::ToolStarted {
                    task_id: None,
                    sequence: None,
                    tool_call_id: None,
                    name: name.clone(),
                    args: format!("args-{i}"),
                });
                let _ = state.reduce(Action::ToolFinished {
                    task_id: None,
                    sequence: None,
                    tool_call_id: None,
                    name,
                    success: ok,
                    duration_ms: 10,
                    result: None,
                });
            }
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "a".to_string(),
            });

            let last = state.session.turns.last().expect("test: assistant turn");
            assert_eq!(last.tool_calls.len(), 3);
            let t0 = last.tool_calls.first().expect("test: tool_calls[0]");
            assert_eq!(t0.name, "tool1");
            assert!(t0.success);
            let t1 = last.tool_calls.get(1).expect("test: tool_calls[1]");
            assert_eq!(t1.name, "tool2");
            assert!(!t1.success);
            let t2 = last.tool_calls.get(2).expect("test: tool_calls[2]");
            assert_eq!(t2.name, "tool3");
            assert!(t2.success);
        }

        /// S2.5 P1-B: turn 边界 (StreamCompleted) 清空缓冲，跨轮不污染.
        #[test]
        fn s2_5_p1_b_turn_boundary_clears_buffer() {
            let mut state = s();

            // Turn 1：tool 累积，但故意不发 RecordAssistantTurn，直接 StreamCompleted.
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-p1b-3a".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "leftover".to_string(),
                args: "x".to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "leftover".to_string(),
                success: true,
                duration_ms: 1,
                result: None,
            });
            assert_eq!(
                state
                    .control
                    .tool_buffers
                    .get(&ToolTaskKey::Primary)
                    .map_or(0, |buffer| buffer.tool_calls.len()),
                1
            );
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d-p1b-3a".to_string(),
                final_text: "x".to_string(),
                reasoning: String::new(),
            });
            // StreamCompleted 兜底 clear 后缓冲为空.
            assert!(!state.control.tool_buffers.contains_key(&ToolTaskKey::Primary));

            // Turn 2：RecordAssistantTurn 应得到空 tool_calls（未被 Turn 1 残留污染）.
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-p1b-3b".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "clean".to_string(),
            });
            let last = state.session.turns.last().expect("test: turn 2 assistant");
            assert_eq!(last.tool_calls.len(), 0, "Turn 2 不能继承 Turn 1 残留 tool_calls");
        }

        /// S2.5 P1-B: stream cancelled 清空缓冲（用户中途 Ctrl+C）.
        #[test]
        fn s2_5_p1_b_stream_cancelled_clears_buffer() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-p1b-4".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "partial".to_string(),
                args: "...".to_string(),
            });
            // 此时 args 暂存有内容
            assert_eq!(
                state
                    .control
                    .tool_buffers
                    .get(&ToolTaskKey::Primary)
                    .map_or(0, |buffer| buffer.tool_args.len()),
                1
            );

            let _ = state.reduce(Action::StreamCancelled {
                draft_id: "d-p1b-4".to_string(),
            });
            assert!(
                !state.control.tool_buffers.contains_key(&ToolTaskKey::Primary),
                "cancel 后缓冲必须清空"
            );

            // 同理验证 StreamFailed.
            let mut state2 = s();
            let _ = state2.reduce(Action::TurnStarted {
                draft_id: "d-p1b-4b".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state2.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "partial2".to_string(),
                args: "...".to_string(),
            });
            let _ = state2.reduce(Action::StreamFailed {
                draft_id: "d-p1b-4b".to_string(),
                err: "timeout".to_string(),
                retryable: true,
            });
            assert!(!state2.control.tool_buffers.contains_key(&ToolTaskKey::Primary));
        }

        /// S2.5 P1-B: 扩展 fixB C1 parity 模式 — RecordAssistantTurn 后
        /// session.turns.last().tool_calls 必须包含本轮 ToolFinished 累计.
        /// 模拟 Both 模式下 reducer 路径的 enriched 包路径，验证 reducer 持久化
        /// 路径已含 tool_calls（关闭 FIXME(S2.5) gap）。
        #[test]
        fn s2_5_p1_b_both_parity_includes_tool_calls() {
            let mut state = s();
            // 模拟完整 turn 包：user → turn started → 多个 tool → assistant.
            let _ = state.reduce(Action::RecordUserTurn("ask".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-parity".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                args: r#"{"q":"x"}"#.to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "search".to_string(),
                success: true,
                duration_ms: 5,
                result: Some("hit".to_string()),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "fetch".to_string(),
                args: "url".to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "fetch".to_string(),
                success: false,
                duration_ms: 30,
                result: Some("404".to_string()),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "done".to_string(),
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d-parity".to_string(),
                final_text: "done".to_string(),
                reasoning: String::new(),
            });

            // legacy session.add_assistant_turn(content, tool_calls) 等价镜像.
            let assistant_turn = state
                .session
                .turns
                .iter()
                .rev()
                .find(|t| t.role == "assistant")
                .expect("test: assistant turn exists");
            assert_eq!(assistant_turn.tool_calls.len(), 2);
            let c0 = assistant_turn.tool_calls.first().expect("test: tool_calls[0]");
            assert_eq!(c0.name, "search");
            assert!(c0.success);
            let c1 = assistant_turn.tool_calls.get(1).expect("test: tool_calls[1]");
            assert_eq!(c1.name, "fetch");
            assert!(!c1.success);

            // 验证 build_session_snapshot 落盘的 turns 也含 tool_calls.
            let snap = state.build_session_snapshot();
            let snap_assistant = snap
                .turns
                .iter()
                .rev()
                .find(|t| t.role == "assistant")
                .expect("test: snapshot assistant turn");
            assert_eq!(
                snap_assistant.tool_calls.len(),
                2,
                "build_session_snapshot 落盘的 turns 必须携带 tool_calls"
            );
        }
    }

    #[cfg(test)]
    mod p3a_task_aware_tool_buffers {
        use super::super::*;
        use crate::chat::action::Action;
        use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskId};
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        fn task_pair() -> ((TurnTaskId, u64), (TurnTaskId, u64)) {
            let mut scheduler = TurnScheduler::new();
            let a = scheduler.enqueue("a", TurnPriority::Normal, 0);
            let b = scheduler.enqueue("b", TurnPriority::Normal, 0);
            let a_seq = scheduler.task(a).expect("test: task a").sequence;
            let b_seq = scheduler.task(b).expect("test: task b").sequence;
            ((a, a_seq), (b, b_seq))
        }

        fn start_task(state: &mut ChatState, task_id: TurnTaskId, sequence: u64, draft_id: &str) {
            let _ = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: Some(task_id),
                provider_turn_sequence: Some(sequence),
                draft_id: draft_id.to_string(),
                history: Vec::new(),
                compaction_config: None,
                cancel: CancellationToken::new(),
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
            });
        }

        fn start_tool(state: &mut ChatState, task_id: TurnTaskId, sequence: u64, name: &str, args: &str) {
            let _ = state.reduce(Action::ToolStarted {
                task_id: Some(task_id),
                sequence: Some(sequence),
                tool_call_id: None,
                name: name.to_string(),
                args: args.to_string(),
            });
        }

        fn finish_tool(state: &mut ChatState, task_id: TurnTaskId, sequence: u64, name: &str, success: bool) {
            let _ = state.reduce(Action::ToolFinished {
                task_id: Some(task_id),
                sequence: Some(sequence),
                tool_call_id: None,
                name: name.to_string(),
                success,
                duration_ms: 7,
                result: Some(format!("{name}-result")),
            });
        }

        #[test]
        fn p3a_cancelled_task_finalizes_only_its_tool_buffer() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a");
            start_task(&mut state, task_b, seq_b, "draft-b");
            start_tool(&mut state, task_a, seq_a, "shell", r#"{"cmd":"sleep 1"}"#);
            start_tool(&mut state, task_b, seq_b, "grep", r#"{"q":"needle"}"#);

            let _ = state.reduce(Action::StreamCancelled {
                draft_id: "draft-a".to_string(),
            });

            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_a)), 0);
            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 1);
            assert!(
                state.ui.conversation_lines.iter().any(|line| matches!(
                    line,
                    crate::chat::tui::ConversationLine::ToolResult {
                        tool_name,
                        status: crate::chat::tui::ToolStatus::Running,
                        ..
                    } if tool_name == "grep"
                )),
                "task B running tool card must survive task A cancellation"
            );
        }

        #[test]
        fn p3a_completed_task_clears_only_matching_buffer() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a");
            start_task(&mut state, task_b, seq_b, "draft-b");
            start_tool(&mut state, task_a, seq_a, "search", r#"{"q":"a"}"#);
            finish_tool(&mut state, task_a, seq_a, "search", true);
            start_tool(&mut state, task_b, seq_b, "fetch", r#"{"url":"b"}"#);

            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "draft-a".to_string(),
                final_text: "a done".to_string(),
                reasoning: String::new(),
            });

            assert_eq!(state.control.tool_call_count(ToolTaskKey::Task(task_a)), 0);
            assert_eq!(state.control.tool_arg_count(ToolTaskKey::Task(task_b)), 1);
            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 1);
        }

        #[test]
        fn p3a_record_assistant_turn_drains_only_requested_task_calls() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a");
            start_task(&mut state, task_b, seq_b, "draft-b");
            start_tool(&mut state, task_a, seq_a, "search", r#"{"q":"a"}"#);
            finish_tool(&mut state, task_a, seq_a, "search", true);
            start_tool(&mut state, task_b, seq_b, "fetch", r#"{"url":"b"}"#);
            finish_tool(&mut state, task_b, seq_b, "fetch", false);

            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: Some(task_b),
                content: "b answer".to_string(),
            });

            let last = state.session.turns.last().expect("test: assistant turn");
            assert_eq!(last.tool_calls.len(), 1);
            let call = last.tool_calls.first().expect("test: tool call");
            assert_eq!(call.name, "fetch");
            assert!(!call.success);
            assert_eq!(call.task_id, Some(task_b.get()));
            assert_eq!(call.sequence, Some(seq_b));
            assert_eq!(state.control.tool_call_count(ToolTaskKey::Task(task_a)), 1);
            assert_eq!(state.control.tool_call_count(ToolTaskKey::Task(task_b)), 0);
        }

        #[test]
        fn p3a_same_tool_name_args_are_isolated_by_task() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a");
            start_task(&mut state, task_b, seq_b, "draft-b");
            start_tool(&mut state, task_a, seq_a, "shell", r#"{"cmd":"echo a"}"#);
            start_tool(&mut state, task_b, seq_b, "shell", r#"{"cmd":"echo b"}"#);

            finish_tool(&mut state, task_b, seq_b, "shell", true);

            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_a)), 1);
            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 0);
            let b_call = state
                .control
                .tool_buffers
                .get(&ToolTaskKey::Task(task_b))
                .and_then(|buffer| buffer.tool_calls.first())
                .expect("test: task b call");
            assert!(b_call.args_preview.contains("echo b"));
            assert_eq!(state.control.tool_arg_count(ToolTaskKey::Task(task_a)), 1);

            finish_tool(&mut state, task_a, seq_a, "shell", true);
            let a_call = state
                .control
                .tool_buffers
                .get(&ToolTaskKey::Task(task_a))
                .and_then(|buffer| buffer.tool_calls.first())
                .expect("test: task a call");
            assert!(a_call.args_preview.contains("echo a"));
        }

        #[test]
        fn p3a_legacy_primary_tool_buffer_path_still_records_tool_calls() {
            let mut state = s();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "primary-draft".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                args: r#"{"cmd":"ls"}"#.to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "shell".to_string(),
                success: true,
                duration_ms: 3,
                result: Some("ok".to_string()),
            });
            let _ = state.reduce(Action::RecordAssistantTurn {
                task_id: None,
                content: "primary answer".to_string(),
            });

            let last = state.session.turns.last().expect("test: primary assistant turn");
            assert_eq!(last.tool_calls.len(), 1);
            let call = last.tool_calls.first().expect("test: primary tool call");
            assert_eq!(call.name, "shell");
            assert!(call.success);
            assert_eq!(call.task_id, None);
            assert_eq!(call.sequence, None);
            assert!(!state.control.tool_buffers.contains_key(&ToolTaskKey::Primary));
        }
    }

    #[cfg(test)]
    mod p3b_task_aware_cancel_tokens {
        use super::super::*;
        use crate::chat::action::Action;
        use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskId};
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        fn task_pair() -> ((TurnTaskId, u64), (TurnTaskId, u64)) {
            let mut scheduler = TurnScheduler::new();
            let a = scheduler.enqueue("a", TurnPriority::Normal, 0);
            let b = scheduler.enqueue("b", TurnPriority::Normal, 0);
            let a_seq = scheduler.task(a).expect("test: task a").sequence;
            let b_seq = scheduler.task(b).expect("test: task b").sequence;
            ((a, a_seq), (b, b_seq))
        }

        fn start_task(
            state: &mut ChatState,
            task_id: TurnTaskId,
            sequence: u64,
            draft_id: &str,
            cancel: CancellationToken,
        ) {
            let _ = state.reduce(Action::StartLLMTurn {
                provider_turn_task_id: Some(task_id),
                provider_turn_sequence: Some(sequence),
                draft_id: draft_id.to_string(),
                history: Vec::new(),
                compaction_config: None,
                cancel,
                turn_spawn_ctx: None,
                turn_message_send_ctx: None,
            });
        }

        fn cancel_tokens(effects: Vec<Effect>) -> Vec<CancellationToken> {
            effects
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::CancelToken(token) => Some(token),
                    _ => None,
                })
                .collect()
        }

        #[test]
        fn p3b_two_task_tokens_are_independent() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            let token_a = CancellationToken::new();
            let token_b = CancellationToken::new();
            start_task(&mut state, task_a, seq_a, "draft-a", token_a.clone());
            start_task(&mut state, task_b, seq_b, "draft-b", token_b.clone());

            let tokens = cancel_tokens(state.reduce(Action::CancelRequested));

            assert_eq!(tokens.len(), 1);
            for token in tokens {
                token.cancel();
            }
            assert!(token_a.is_cancelled(), "primary task A token must be emitted");
            assert!(!token_b.is_cancelled(), "task B token must remain untouched");
            assert!(!state.control.turn_cancels.contains_key(&task_a));
            assert!(state.control.turn_cancels.contains_key(&task_b));
        }

        #[test]
        fn p3b_cancel_primary_does_not_clear_other_task_tool_buffer() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a", CancellationToken::new());
            start_task(&mut state, task_b, seq_b, "draft-b", CancellationToken::new());
            let _ = state.reduce(Action::ToolStarted {
                task_id: Some(task_b),
                sequence: Some(seq_b),
                tool_call_id: None,
                name: "grep".to_string(),
                args: r#"{"q":"needle"}"#.to_string(),
            });

            let _ = state.reduce(Action::CancelRequested);

            assert!(state.control.generating, "task B still keeps generation active");
            assert!(
                state
                    .stream
                    .visible_drafts
                    .iter()
                    .any(|draft| draft.task_id == Some(task_b))
            );
            assert_eq!(state.control.pending_tool_card_count(ToolTaskKey::Task(task_b)), 1);
            assert!(state.control.turn_cancels.contains_key(&task_b));
        }

        #[test]
        fn p3b_global_state_clears_only_after_all_visible_tasks_are_cancelled() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            start_task(&mut state, task_a, seq_a, "draft-a", CancellationToken::new());
            start_task(&mut state, task_b, seq_b, "draft-b", CancellationToken::new());
            let _ = state.reduce(Action::ToolApprovalRequested {
                task_id: Some(task_b),
                tool_id: "tool-b".to_string(),
                name: "shell".to_string(),
                args: "{}".to_string(),
            });

            let _ = state.reduce(Action::CancelRequested);

            assert!(state.control.generating, "B remains visible after cancelling A");
            assert!(
                state.control.active_cancel.is_none(),
                "task turns do not use legacy active_cancel"
            );
            assert!(
                state.ui.pending_tool_approval.is_some(),
                "B approval must not be globally cleared"
            );
            assert!(state.control.turn_cancels.contains_key(&task_b));

            let _ = state.reduce(Action::CancelRequested);

            assert!(!state.control.generating);
            assert!(state.control.active_cancel.is_none());
            assert!(state.control.turn_cancels.is_empty());
            assert!(state.stream.visible_drafts.is_empty());
            assert!(state.ui.pending_tool_approval.is_none());
        }

        #[test]
        fn p3b_shutdown_cancels_all_task_tokens() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            let token_a = CancellationToken::new();
            let token_b = CancellationToken::new();
            start_task(&mut state, task_a, seq_a, "draft-a", token_a.clone());
            start_task(&mut state, task_b, seq_b, "draft-b", token_b.clone());

            let effects = state.reduce(Action::ShutdownRequested);
            let tokens = cancel_tokens(effects);

            assert_eq!(tokens.len(), 2, "shutdown must emit both task cancel tokens");
            for token in tokens {
                token.cancel();
            }
            assert!(token_a.is_cancelled());
            assert!(token_b.is_cancelled());
            assert!(state.control.turn_cancels.is_empty());
            assert!(!state.control.generating);
            assert!(state.stream.visible_drafts.is_empty());
        }

        #[test]
        fn p4c_cancel_provider_turn_targets_requested_task_token_only() {
            let mut state = s();
            let ((task_a, seq_a), (task_b, seq_b)) = task_pair();
            let token_a = CancellationToken::new();
            let token_b = CancellationToken::new();
            start_task(&mut state, task_a, seq_a, "draft-a", token_a.clone());
            start_task(&mut state, task_b, seq_b, "draft-b", token_b.clone());

            let tokens = cancel_tokens(state.reduce(Action::CancelProviderTurn { task_id: task_b }));

            assert_eq!(
                tokens.len(),
                1,
                "targeted cancel should emit only the requested task token"
            );
            for token in tokens {
                token.cancel();
            }
            assert!(!token_a.is_cancelled(), "peer task token must remain live");
            assert!(token_b.is_cancelled(), "requested task token must be cancelled");
            assert!(
                state.control.turn_cancels.contains_key(&task_a),
                "peer task cancel token remains retained"
            );
            assert!(
                !state.control.turn_cancels.contains_key(&task_b),
                "requested task cancel token is consumed"
            );
            assert!(
                state
                    .stream
                    .visible_drafts
                    .iter()
                    .any(|draft| draft.task_id == Some(task_a)),
                "peer draft remains visible"
            );
            assert!(
                state
                    .stream
                    .visible_drafts
                    .iter()
                    .all(|draft| draft.task_id != Some(task_b)),
                "requested draft is removed"
            );
            assert!(state.control.generating, "peer task keeps generation active");
        }

        #[test]
        fn p3b_single_legacy_turn_ctrl_c_still_uses_primary_active_cancel() {
            let mut state = s();
            let token = CancellationToken::new();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "primary-draft".to_string(),
                cancel: token.clone(),
            });

            let effects = state.reduce(Action::CancelRequested);
            let tokens = cancel_tokens(effects);

            assert_eq!(tokens.len(), 1);
            for token in tokens {
                token.cancel();
            }
            assert!(token.is_cancelled());
            assert!(state.control.active_cancel.is_none());
            assert!(state.control.turn_cancels.is_empty());
            assert!(!state.control.generating);
            assert!(state.stream.primary_streaming_draft().is_none());
        }
    }

    #[cfg(test)]
    mod p3c_task_aware_usage {
        use super::super::*;
        use crate::chat::action::{Action, ProviderUsageRecordKind};
        use crate::chat::turn_scheduler::{TurnPriority, TurnScheduler, TurnTaskId};
        use crate::llm::route_decision::TokenUsageSource;
        use tokio_util::sync::CancellationToken;

        fn s() -> ChatState {
            ChatState::new(Arc::from("openai"), Arc::from("gpt-4o-mini"), CancellationToken::new())
        }

        fn task_pair() -> (TurnTaskId, TurnTaskId) {
            let mut scheduler = TurnScheduler::new();
            let a = scheduler.enqueue("usage-a", TurnPriority::Normal, 0);
            let b = scheduler.enqueue("usage-b", TurnPriority::Normal, 0);
            (a, b)
        }

        fn record(total_tokens: u64) -> MainSessionTokenUsageRecord {
            MainSessionTokenUsageRecord {
                provider: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                prompt_tokens: total_tokens / 2,
                completion_tokens: total_tokens - (total_tokens / 2),
                total_tokens,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                source: TokenUsageSource::Reported,
                cost_usd: None,
            }
        }

        fn provider_usage(
            state: &mut ChatState,
            task_id: Option<TurnTaskId>,
            usage_kind: ProviderUsageRecordKind,
            total_tokens: u64,
        ) -> Vec<Effect> {
            state.reduce(Action::ProviderUsageRecorded {
                task_id,
                usage_kind,
                record: record(total_tokens),
            })
        }

        #[test]
        fn p3c_same_task_final_aggregate_is_deduped_once() {
            let mut state = s();
            let (task_id, _) = task_pair();

            let first = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::FinalAggregate, 10);
            let second = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::FinalAggregate, 20);

            assert_eq!(first.len(), 2);
            assert!(second.is_empty(), "duplicate final aggregate should be a reducer no-op");
            assert_eq!(state.session.token_usage_records.len(), 1);
            assert_eq!(state.ui.token_usage_summary.total_tokens, 10);
        }

        #[test]
        fn p3c_out_of_order_final_usage_stays_on_own_task() {
            let mut state = s();
            let (task_a, task_b) = task_pair();

            let _ = provider_usage(&mut state, Some(task_b), ProviderUsageRecordKind::FinalAggregate, 40);
            let _ = provider_usage(&mut state, Some(task_a), ProviderUsageRecordKind::FinalAggregate, 15);
            let duplicate_b = provider_usage(&mut state, Some(task_b), ProviderUsageRecordKind::FinalAggregate, 99);

            assert!(duplicate_b.is_empty());
            assert_eq!(state.session.token_usage_records.len(), 2);
            let totals = state
                .session
                .token_usage_records
                .iter()
                .map(|record| record.total_tokens)
                .collect::<Vec<_>>();
            assert_eq!(totals, vec![40, 15]);
            assert_eq!(state.ui.token_usage_summary.total_tokens, 55);
            assert!(state.control.final_usage_tasks_recorded.contains(&task_a));
            assert!(state.control.final_usage_tasks_recorded.contains(&task_b));
        }

        #[test]
        fn p3c_incremental_usage_for_same_task_is_not_deduped() {
            let mut state = s();
            let (task_id, _) = task_pair();

            let _ = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::Incremental, 7);
            let _ = provider_usage(&mut state, Some(task_id), ProviderUsageRecordKind::Incremental, 11);

            assert_eq!(state.session.token_usage_records.len(), 2);
            assert_eq!(state.ui.token_usage_summary.request_count, 2);
            assert_eq!(state.ui.token_usage_summary.total_tokens, 18);
            assert!(state.control.final_usage_tasks_recorded.is_empty());
        }

        #[test]
        fn p3c_legacy_usage_without_task_id_is_not_deduped() {
            let mut state = s();

            let _ = provider_usage(&mut state, None, ProviderUsageRecordKind::FinalAggregate, 12);
            let _ = provider_usage(&mut state, None, ProviderUsageRecordKind::FinalAggregate, 13);

            assert_eq!(state.session.token_usage_records.len(), 2);
            assert_eq!(state.ui.token_usage_summary.request_count, 2);
            assert_eq!(state.ui.token_usage_summary.total_tokens, 25);
            assert!(state.control.final_usage_tasks_recorded.is_empty());
        }
    }

    // ─── S4-A Commit 1: UiSnapshot + reduce_tracked 单测 ─────────────────────

    #[cfg(feature = "terminal-tui")]
    mod s4_a_1 {
        use super::*;
        use crate::chat::tui::ConversationLine;
        use tokio_util::sync::CancellationToken;

        fn make_state() -> ChatState {
            ChatState::new(
                Arc::from("test-provider"),
                Arc::from("test-model"),
                CancellationToken::new(),
            )
        }

        #[test]
        fn s4_a_1_snapshot_initial_zero_revision() {
            let snap = UiSnapshot::initial(Arc::from("p"), Arc::from("m"));
            assert_eq!(snap.revision, 0);
            assert_eq!(&*snap.provider, "p");
            assert_eq!(&*snap.model, "m");
            assert!(snap.conversation_lines.is_empty());
            assert_eq!(snap.turn_count, 0);
            assert!(snap.streaming.is_none());
        }

        #[test]
        fn s4_a_1_snapshot_clone_is_arc_shallow() {
            // 验证 conversation_lines 是 Arc 共享：clone snapshot 后两份 Arc
            // 指向同一底层 Vec，strong_count 至少为 2.
            let mut state = make_state();
            state.ui.conversation_lines.push(ConversationLine::User {
                content: "hi".to_string(),
            });
            let snap = state.build_ui_snapshot(1);
            let snap2 = snap.clone();
            // Arc::strong_count(&snap.conversation_lines) 包含 snap + snap2 = 2
            assert!(
                Arc::strong_count(&snap.conversation_lines) >= 2,
                "snapshot clone 应共享 conversation_lines Arc, count={}",
                Arc::strong_count(&snap.conversation_lines)
            );
            assert_eq!(snap2.revision, 1);
        }

        #[test]
        fn s4_a_1_build_after_user_message_includes_line() {
            let mut state = make_state();
            state.ui.conversation_lines.push(ConversationLine::User {
                content: "hello".to_string(),
            });
            let snap = state.build_ui_snapshot(7);
            assert_eq!(snap.revision, 7);
            assert_eq!(snap.conversation_lines.len(), 1);
            match snap.conversation_lines.first() {
                Some(ConversationLine::User { content }) => assert_eq!(content, "hello"),
                other => panic!("expected User line, got {other:?}"),
            }
        }

        #[test]
        fn s4_a_1_ui_dirty_true_on_record_user_turn_via_runtime_fallback() {
            // RecordUserTurn 静态判定 false（写 session 不写 ui）；
            // 运行时 snapshot_dirty_fields 也不变 → 整体 false.
            // 此用例校验：写 session 不连带触发 dirty.
            let mut state = make_state();
            let (_effects, dirty) = state.reduce_tracked(Action::RecordUserTurn("q".into()));
            assert!(!dirty, "RecordUserTurn 不应触发 ui_dirty");
        }

        #[test]
        fn s4_a_1_tool_progress_dirty_but_retry_trace_only_is_clean() {
            let mut state = make_state();
            let (_e, d) = state.reduce_tracked(Action::ToolProgress { iteration: 1, max: 3 });
            assert!(d, "ToolProgress must dirty Pure snapshots so progress is visible");
            let (_e, d2) = state.reduce_tracked(Action::StreamRetryAttempt {
                attempt: 1,
                reason: "x".into(),
            });
            assert!(!d2, "StreamRetryAttempt 不应 dirty");
        }

        #[test]
        fn s4_a_1_ui_dirty_true_on_stream_completed() {
            // 完整流程：先 TurnStarted 注册 draft，再 StreamCompleted finalize.
            let mut state = make_state();
            let token = CancellationToken::new();
            let (_e, d_start) = state.reduce_tracked(Action::TurnStarted {
                draft_id: "d1".into(),
                cancel: token,
            });
            assert!(d_start, "TurnStarted 应 dirty (stream.draft 变化)");
            let (_e, d_done) = state.reduce_tracked(Action::StreamCompleted {
                draft_id: "d1".into(),
                final_text: "hi".into(),
                reasoning: String::new(),
            });
            assert!(d_done, "StreamCompleted 应 dirty (conversation_lines + stream.draft)");
        }

        #[test]
        fn s4_a_1_ui_dirty_true_on_system_message_added() {
            let mut state = make_state();
            let (_e, d) = state.reduce_tracked(Action::SystemMessageAdded { text: "banner".into() });
            assert!(d, "SystemMessageAdded 应 dirty (push 到 conversation_lines)");
        }

        #[test]
        fn s4_a_1_build_session_title_into_arc() {
            // session.title 是 String，snapshot 内是 Arc<str> — 验证转换正确.
            let mut state = make_state();
            state.session.title = "my chat".to_string();
            let snap = state.build_ui_snapshot(2);
            assert_eq!(&*snap.session_title, "my chat");
        }
    }

    // ─── S4-A Commit 6: dual-path parity (mirror vs reducer) ───────────────

    #[cfg(feature = "terminal-tui")]
    mod s4_a_6 {
        use super::*;
        use crate::chat::tui::{ConversationLine, ToolStatus, TuiState};
        use tokio_util::sync::CancellationToken;

        /// 双跑对账：构造同一 Action 序列, 分别灌入 mirror 路径 (TuiState 直接
        /// 调用 push_*) 与 reducer 路径 (Action → reduce → ui.conversation_lines),
        /// 断言两路径输出 conversation_lines 字节级一致.
        ///
        /// 这是 S4-B 真删 chat_mirror 前的最后保险 — 任何 reducer 行为偏离 legacy
        /// mirror 都会在这里被字节级 diff 出来.
        #[test]
        fn s4_a_6_dual_path_parity_user_assistant_tool() {
            // ── 路径 A: mirror 路径 ──
            let mut mirror = TuiState::new("p", "m");
            mirror.push_system_message("banner");
            mirror.push_user_message("hello");
            mirror.push_tool_result_started("Bash", "{\"cmd\":\"ls\"}");
            let _ = mirror.mark_last_tool_result_finished("Bash", true, 50, None);
            mirror.push_assistant_message("done");

            // ── 路径 B: reducer 路径 (S4-A Commit A: UserMessageEchoed 闭合 User echo) ──
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let _ = state.reduce(Action::SystemMessageAdded {
                text: "banner".to_string(),
            });
            let _ = state.reduce(Action::UserMessageEchoed("hello".to_string()));
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "Bash".to_string(),
                args: "{\"cmd\":\"ls\"}".to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "Bash".to_string(),
                success: true,
                duration_ms: 50,
                result: None,
            });
            let token = CancellationToken::new();
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-1".to_string(),
                cancel: token,
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d-1".to_string(),
                final_text: "done".to_string(),
                reasoning: String::new(),
            });

            // 字节级对账：四类 ConversationLine (System/User/ToolResult/Assistant) 全对齐
            let mirror_lines: Vec<&ConversationLine> = mirror.conversation_lines.iter().collect();
            let reducer_lines: Vec<&ConversationLine> = state.ui.conversation_lines.iter().collect();

            assert_eq!(
                mirror_lines.len(),
                reducer_lines.len(),
                "对账行数: mirror={}, reducer={}",
                mirror_lines.len(),
                reducer_lines.len()
            );
            for (i, (ml, rl)) in mirror_lines.iter().zip(reducer_lines.iter()).enumerate() {
                let m_dbg = format!("{ml:?}");
                let r_dbg = format!("{rl:?}");
                match (ml, rl) {
                    (
                        ConversationLine::ToolResult {
                            tool_name: m_name,
                            status: m_st,
                            elapsed_ms: m_ms,
                            ..
                        },
                        ConversationLine::ToolResult {
                            tool_name: r_name,
                            status: r_st,
                            elapsed_ms: r_ms,
                            ..
                        },
                    ) => {
                        assert_eq!(m_name, r_name, "line {i} ToolResult tool_name mismatch");
                        assert_eq!(m_st, r_st, "line {i} ToolResult status mismatch");
                        assert_eq!(m_ms, r_ms, "line {i} ToolResult elapsed_ms mismatch");
                        assert_eq!(*m_st, ToolStatus::Done);
                    }
                    (ConversationLine::Assistant { content: mc }, ConversationLine::Assistant { content: rc }) => {
                        assert_eq!(mc, rc, "line {i} Assistant content mismatch");
                    }
                    (ConversationLine::System { content: mc }, ConversationLine::System { content: rc }) => {
                        assert_eq!(mc, rc, "line {i} System content mismatch");
                    }
                    (ConversationLine::User { content: mc }, ConversationLine::User { content: rc }) => {
                        assert_eq!(mc, rc, "line {i} User content mismatch");
                    }
                    _ => panic!("line {i} variant 不匹配: mirror={m_dbg}, reducer={r_dbg}"),
                }
            }
        }

        /// S4-A Commit A: Pure 模式下 UserMessageEchoed 把 User 行写入 conversation_lines
        #[test]
        fn s4_a_post_p0_pure_user_echo_appears_in_snapshot() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let effects = state.reduce(Action::UserMessageEchoed("hello echo".to_string()));
            assert!(
                effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
                "UserMessageEchoed 应 emit RequestRedraw"
            );
            let snap = state.build_ui_snapshot(0);
            let last_user = snap
                .conversation_lines
                .iter()
                .find_map(|l| match l {
                    ConversationLine::User { content } => Some(content.as_str()),
                    _ => None,
                })
                .expect("snapshot 应含 User 行");
            assert_eq!(last_user, "hello echo");
        }

        /// S4-B T4-B-6: created_at 严格语义 — 首次 RecordUserTurn 初始化 + 多 turn 保持不变
        #[test]
        fn s4_b_created_at_stable_across_turns() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            state.session.id = "sess1".to_string();
            assert!(state.session.created_at.is_none(), "初始 created_at 应为 None");

            let _ = state.reduce(Action::RecordUserTurn("q1".to_string()));
            let created_first = state.session.created_at.expect("首次 RecordUserTurn 应设置 created_at");

            // 第二次 RecordUserTurn 不应覆盖 created_at
            std::thread::sleep(std::time::Duration::from_millis(2));
            let _ = state.reduce(Action::RecordUserTurn("q2".to_string()));
            assert_eq!(
                state.session.created_at,
                Some(created_first),
                "多 turn 不应覆盖 created_at"
            );

            // build_session_snapshot 应使用 SessionState.created_at
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d".to_string(),
                cancel: CancellationToken::new(),
            });
            let effects = state.reduce(Action::StreamCompleted {
                draft_id: "d".to_string(),
                final_text: "ok".to_string(),
                reasoning: String::new(),
            });
            let snap_session = effects
                .iter()
                .find_map(|e| match e {
                    Effect::SaveSession(s) => Some(s),
                    _ => None,
                })
                .expect("StreamCompleted 应 emit SaveSession");
            assert_eq!(
                snap_session.created_at, created_first,
                "build_session_snapshot 应继承 SessionState.created_at 不覆盖"
            );
        }

        /// S4-B T4-B-4：route_turn 在 Pure 模式下总返回 ReduxDriver
        #[test]
        fn s4_b_route_turn_pure_always_redux_driver() {
            use crate::chat::{ReduxMode, TurnRoute, route_turn};
            assert_eq!(
                route_turn(ReduxMode::Pure),
                TurnRoute::ReduxDriver,
                "Pure 模式无需 driver_opt_in 也应路由到 ReduxDriver"
            );
        }

        /// S4-B 删除清理：mirror push 全删后 reducer 单源接管 4 类 ConversationLine push
        #[test]
        fn s4_b_reducer_sole_source_for_conversation_lines() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let _ = state.reduce(Action::SystemMessageAdded {
                text: "banner".to_string(),
            });
            let _ = state.reduce(Action::UserMessageEchoed("hi".to_string()));
            let _ = state.reduce(Action::ToolStarted {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "Bash".to_string(),
                args: "{}".to_string(),
            });
            let _ = state.reduce(Action::ToolFinished {
                task_id: None,
                sequence: None,
                tool_call_id: None,
                name: "Bash".to_string(),
                success: true,
                duration_ms: 10,
                result: None,
            });
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d".to_string(),
                cancel: CancellationToken::new(),
            });
            let _ = state.reduce(Action::StreamCompleted {
                draft_id: "d".to_string(),
                final_text: "ok".to_string(),
                reasoning: String::new(),
            });
            assert_eq!(state.ui.conversation_lines.len(), 4, "reducer 单源应 push 4 行");
            let mut iter = state.ui.conversation_lines.iter();
            assert!(matches!(iter.next(), Some(ConversationLine::System { .. })));
            assert!(matches!(iter.next(), Some(ConversationLine::User { .. })));
            assert!(matches!(iter.next(), Some(ConversationLine::ToolResult { .. })));
            assert!(matches!(iter.next(), Some(ConversationLine::Assistant { .. })));
        }

        /// S4-A Commit E: TerminalResized 不应标 ui_dirty (snapshot 字段集不变, redraw 走 Effect 路径)
        #[test]
        fn s4_a_post_p2_terminal_resized_not_dirty() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            let (effects, dirty) = state.reduce_tracked(Action::TerminalResized { w: 120, h: 40 });
            assert!(
                effects.iter().any(|e| matches!(e, Effect::RequestRedraw)),
                "TerminalResized 仍应 emit RequestRedraw (redraw 走 redraw_tx)"
            );
            assert!(
                !dirty,
                "TerminalResized 不动 snapshot 字段，dirty 应 false 避免无意义 watch push"
            );
        }

        /// S4-A Commit B: 连续两次 build_ui_snapshot 未变 ui 时 Arc::ptr_eq 共享
        #[test]
        fn s4_a_post_p1_arc_shared_no_clone() {
            let mut state = ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new());
            // 先 push 一行让 conversation_lines 非空 + 缓存命中
            let _ = state.reduce(Action::SystemMessageAdded {
                text: "banner".to_string(),
            });
            let snap1 = state.build_ui_snapshot(1);
            let snap2 = state.build_ui_snapshot(2);
            assert!(
                Arc::ptr_eq(&snap1.conversation_lines, &snap2.conversation_lines),
                "未变 ui 时连续 build_ui_snapshot 应共享 Arc，避免每帧 O(n) 克隆"
            );

            // dirty Action 后缓存应被清空，新 Arc 指针不同
            let _ = state.reduce_tracked(Action::SystemMessageAdded {
                text: "second".to_string(),
            });
            let snap3 = state.build_ui_snapshot(3);
            assert!(
                !Arc::ptr_eq(&snap2.conversation_lines, &snap3.conversation_lines),
                "ui 变更后缓存失效，新快照应是新 Arc"
            );
            assert_eq!(snap3.conversation_lines.len(), 2, "新快照应含两行");
        }
    }

    /// S5 不变量测试套件：reducer 的核心 invariants（顺序 / 幂等 / 取消）
    #[cfg(feature = "terminal-tui")]
    mod s5_invariants {
        use super::super::{ChatState, Effect};
        use crate::chat::action::Action;
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        fn fresh_state() -> ChatState {
            ChatState::new(Arc::from("p"), Arc::from("m"), CancellationToken::new())
        }

        /// 顺序不变量：相同 Action 序列在两个全新 state 上 reduce → 相同终态
        #[test]
        fn s5_invariant_determinism_same_actions_same_state() {
            let actions = || -> Vec<Action> {
                vec![
                    Action::SystemMessageAdded {
                        text: "banner".to_string(),
                    },
                    Action::UserMessageEchoed("hi".to_string()),
                    Action::RecordUserTurn("hi".to_string()),
                    Action::TurnStarted {
                        draft_id: "d1".to_string(),
                        cancel: CancellationToken::new(),
                    },
                    Action::RecordAssistantTurn {
                        task_id: None,
                        content: "ok".to_string(),
                    },
                    Action::StreamCompleted {
                        draft_id: "d1".to_string(),
                        final_text: "ok".to_string(),
                        reasoning: String::new(),
                    },
                ]
            };
            let mut state_a = fresh_state();
            let mut state_b = fresh_state();
            for a in actions() {
                let _ = state_a.reduce(a);
            }
            for a in actions() {
                let _ = state_b.reduce(a);
            }
            assert_eq!(
                state_a.session.turns.len(),
                state_b.session.turns.len(),
                "相同 Action 序列应得到相同 turns 数"
            );
            assert_eq!(
                state_a.ui.conversation_lines.len(),
                state_b.ui.conversation_lines.len(),
                "相同 Action 序列应得到相同 ConversationLines 数"
            );
            assert_eq!(
                state_a.session.title, state_b.session.title,
                "相同 Action 序列应产生相同 session title"
            );
        }

        /// 幂等不变量：重复 dispatch 同一 Action 不应在持久化路径双写
        #[test]
        fn s5_invariant_idempotent_duplicate_dispatch() {
            let mut state = fresh_state();
            let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
            let turns_after_first = state.session.turns.len();
            let history_after_first = state.session.history.len();
            // 实际架构里重复 dispatch 会双写 — 这是 reducer 当前行为契约，本测试锁定它
            let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
            assert!(
                state.session.turns.len() > turns_after_first,
                "RecordUserTurn 非幂等：重复 dispatch 会追加新 turn（chat::run 保证不重复 dispatch）"
            );
            assert!(state.session.history.len() > history_after_first, "history 同样会追加");
        }

        /// 取消不变量：CancelRequested 后无 SaveSession effect（不写 partial state）
        #[test]
        fn s5_invariant_cancel_no_partial_save() {
            let mut state = fresh_state();
            let _ = state.reduce(Action::RecordUserTurn("q".to_string()));
            let _ = state.reduce(Action::TurnStarted {
                draft_id: "d-cancel".to_string(),
                cancel: CancellationToken::new(),
            });
            // 用户中途取消
            let cancel_effects = state.reduce(Action::CancelRequested);
            assert!(
                !cancel_effects.iter().any(|e| matches!(e, Effect::SaveSession(_))),
                "CancelRequested 不应 emit SaveSession（避免 partial state 持久化）"
            );
            // StreamCancelled 也不应 emit SaveSession
            let stream_cancel_effects = state.reduce(Action::StreamCancelled {
                draft_id: "d-cancel".to_string(),
            });
            assert!(
                !stream_cancel_effects
                    .iter()
                    .any(|e| matches!(e, Effect::SaveSession(_))),
                "StreamCancelled 不应 emit SaveSession（附录 B Cancelled 行）"
            );
        }
    }
}
