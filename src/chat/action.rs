//! Redux-like Action 代数. 所有状态变更必须通过 reduce 应用.
//!
//! [`Action`] 是单一事件代数，覆盖 chat 主循环中所有状态变更点（共 23 个变体）。
//! 设计原则:
//! - 所有变体必须 `Send + Sync`（通过 channel 跨任务传递）
//! - 不携带 Provider/Memory/Channel 句柄（那些是 Effect 执行器的依赖）
//! - Clone 而非 Copy（含 String/CancellationToken）

use crossterm::event::KeyEvent;
use tokio_util::sync::CancellationToken;

use crate::agent::loop_::ChatMode;
use crate::chat::session::ChatSession;

/// 历史导航方向。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum HistoryDir {
    /// 向上（更旧）
    Up,
    /// 向下（更新）
    Down,
}

/// 历史 compaction 触发原因（用于 trace / 测试断言）.
///
/// S2-B Step 1: 加入 `HistoryCompacted` Action 时同步引入，让 reducer
/// 能在 trace 里区分是 context-overflow 自动 compaction 还是用户/测试手动触发，
/// 同时让单元测试断言 reason 字段穿透 Effect::LogTrace 输出。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactReason {
    /// context window 超限自动 compaction（chat::run 主循环 overflow 重试路径）.
    ContextOverflow,
    /// 用户手动触发（如 /compact 命令，预留给后续 step）.
    Manual,
}

/// 单一事件代数，所有状态变更必须通过 reduce 应用.
///
/// `Send + Sync`（通过 channel 跨任务传递），无 `Box<dyn>` 即可表达全部 case。
/// Step 1: 类型骨架，Step 2-5 逐步接入调用路径。
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum Action {
    // ── 输入路径 ────────────────────────────────────────────────
    /// 键盘原始事件
    KeyPressed(KeyEvent),
    /// 括号粘贴
    PasteReceived(String),
    /// 终端尺寸变化
    TerminalResized { w: u16, h: u16 },
    /// dispatcher 解析出的提交（用户按下 Enter）
    InputSubmitted(String),
    /// Up/Down 历史导航
    HistoryNavigated(HistoryDir),
    /// Esc — 取消当前输入
    InputCancelled,

    // ── 槽命令 ──────────────────────────────────────────────────
    /// 用户输入了斜杠命令（/plan、/clear 等）
    SlashCommandIssued { cmd: String, args: String },
    /// 模式切换（/plan /edit /auto）
    ModeChanged(ChatMode),
    /// 模型在线切换（/model <name>）— BUG-07.
    ///
    /// reducer 更新 `session.model`，让 status bar 立刻反映新 model；真正影响
    /// 后续 LLM turn 的 model 由主循环把新值写入 `EffectDeps` 的热替换 slot
    /// （同 provider 换 model）。仅记账 + RequestRedraw，不产生其他副作用。
    ModelChanged { model: String },
    /// Provider 在线切换（/provider <name> [model]）— Bug #3.
    ///
    /// reducer 更新 `session.provider`（必要时连带 `session.model`），让 status bar
    /// 与会话快照立刻反映新 provider；真正影响后续 LLM turn 的 provider 实例由主循环
    /// 重建并写入 `ProviderSlot` 热替换 slot 完成（reducer 不持有 provider 实例，故只
    /// 负责 UI / session 账本）。`model` 为 `Some` 时表示切换同时改了 model（命令带了
    /// 兼容 model 参数或当前 model 已变），reducer 一并同步 `session.model`。
    ProviderChanged { provider: String, model: Option<String> },
    /// 清除历史（/clear /new）
    HistoryCleared,
    /// 清除历史并在同一 UI snapshot 中追加用户可见回执。
    HistoryClearedWithNotice { notice: String },

    // ── LLM 流式 ────────────────────────────────────────────────
    /// 新一轮 LLM 推理开始，携带 draft_id 和取消令牌
    TurnStarted {
        draft_id: String,
        cancel: CancellationToken,
    },
    /// Step 5a-3 Phase A — 真主导路径：发起 LLM 流式 turn.
    ///
    /// 与 [`Self::TurnStarted`] 的关系:
    /// - `TurnStarted` 是历史 Action，仅用于 reducer 状态初始化（设置 draft、注册
    ///   active_cancel、置位 generating），由 chat::run 主循环在调用旧 `run_tool_call_loop`
    ///   之前同步投递；不发射 `Effect::StartTurn`
    /// - `StartLLMTurn` 携带完整 `history` 快照，reducer 在初始化 draft 之外**同时**
    ///   发射 `Effect::StartTurn { draft_id, history, cancel }`，由 EffectExecutor
    ///   真接 `provider.stream_chat_with_history`
    ///
    /// Phase A 阶段两者并存，主循环仍由旧路径主导；Phase B 之后旧路径删除，
    /// `TurnStarted` 由 `StartLLMTurn` 完全取代。
    StartLLMTurn {
        draft_id: String,
        history: Vec<crate::providers::ChatMessage>,
        cancel: CancellationToken,
        /// D8-4 (redux path): the turn-root spawn execution context seeded by
        /// `chat::run` for this turn. Threaded through the reducer into
        /// `Effect::StartTurn` so the Redux driver can `SPAWN_EXECUTION_CONTEXT
        /// .scope(...)` its tool-call loop. Without this, sub-agents spawned via
        /// `sessions_spawn` on the redux path see `parent_run_id = None` and are
        /// mislabeled as user-originated instead of model-originated.
        ///
        /// `None` means "no turn-root context" (e.g. tests, or callers that do
        /// not originate a chat turn) — sub-agents then fall back to user origin,
        /// which is correct for non-turn paths such as the `/bg` slash command.
        turn_spawn_ctx: Option<crate::tools::sessions_spawn::SpawnExecutionContext>,
    },
    /// 收到一个 streaming 增量块
    StreamChunkReceived {
        draft_id: String,
        delta: String,
        version: u64,
    },
    /// streaming 完成，携带最终文本和 reasoning 摘要
    StreamCompleted {
        draft_id: String,
        final_text: String,
        reasoning: String,
    },
    /// streaming 失败
    StreamFailed {
        draft_id: String,
        err: String,
        retryable: bool,
    },
    /// streaming 被取消
    StreamCancelled { draft_id: String },

    // ── 工具事件 ────────────────────────────────────────────────
    /// 工具调用开始
    ToolStarted { name: String, args: String },
    /// 工具调用结束
    ToolFinished {
        name: String,
        success: bool,
        duration_ms: u64,
        result: Option<String>,
    },
    /// 工具调用进度（iteration/max）
    ToolProgress { iteration: usize, max: usize },
    /// **S3 T3-1**: driver 请求 UI 对某工具调用做 approval（supervised autonomy 模式下触发）.
    ///
    /// reducer 仅产生 `Effect::RequestApproval`，driver 自身通过 oneshot rx 等响应。
    /// `tool_id` 即 LLM 给出的 `tool_call_id`，用于将响应 [`Self::ToolApprovalReceived`]
    /// 关联到具体 pending oneshot。
    ToolApprovalRequested {
        tool_id: String,
        name: String,
        args: String,
    },
    /// **S3 T3-1**: UI / EffectExecutor 把用户审批结果回投给 driver.
    ///
    /// dispatcher 接收此 Action 时将通过 `approval_response_tx`（注入到 driver 的
    /// 单 mpsc 入口）把决策转给等待中的 driver；driver 端按 `tool_id` 对应到
    /// pending oneshot::Sender<bool> 并 resolve。
    ToolApprovalReceived { tool_id: String, approved: bool },
    /// **S3 T3-1**: 网络瞬时故障重试尝试通知（仅作 UI / trace 用，不变状态）.
    ///
    /// `attempt` 从 1 起计数（第 1 次失败 → attempt=1 之后开始 sleep 重试）。
    /// 失败原因放在 `reason`，便于 UI 显示。
    StreamRetryAttempt { attempt: u8, reason: String },

    // ── 会话 ────────────────────────────────────────────────────
    /// 会话加载完毕
    SessionLoaded(ChatSession),
    /// 会话已持久化
    SessionSaved { id: String },
    /// 切换到指定会话
    SessionSwitched { id: String },
    /// 请求 reducer 持久化用户回合（写入 session.turns + LLM history）
    RecordUserTurn(String),
    /// 请求 reducer 持久化助手回合（写入 session.turns + LLM history）
    RecordAssistantTurn(String),
    /// 请求 reducer append 一条 system 消息到 `session.history`（用于 `/clear` 后
    /// 重建 system prompt 等场景）.
    ///
    /// S2-C Step 2: 与 legacy `history.push(ChatMessage::system(...))` 对齐。
    /// 仅做 append — 不做 upsert，覆盖首位 system 的场景请用
    /// [`Self::SetLeadingSystemPrompt`].
    RecordSystemMessage { content: String },
    /// 请求 reducer set/replace 首位 system prompt — 若 history 为空则 push，
    /// 否则替换 `history[0]`（要求其为 system role）.
    ///
    /// S2-C Step 2: 与 chat::mod 主循环 `if history.is_empty() { push } else {
    /// first_mut = system }` 语义对齐 — 这是每轮 turn 都会跑的 system prompt
    /// 重建路径（technique selection 后的 prompt 注入），不能用 append 表达。
    SetLeadingSystemPrompt { content: String },
    /// 请求 reducer 对 LLM context history 做 compaction（保留 system + 近 N 条 + 总预算）.
    ///
    /// S2-B Step 1: 与 chat::mod 的 `compact_chat_history` 语义对齐 —
    /// reducer 内完成 truncation，无副作用，只产生 LogTrace。`reason` 字段供
    /// 测试断言与 trace 区分 context-overflow vs manual 路径。
    HistoryCompacted { reason: CompactReason },

    // ── UI 折叠/展开 ───────────────────────────────────────────
    /// Tab — 折叠/展开工具卡片
    ToolCardFoldToggled,
    /// Ctrl+R — 折叠/展开 reasoning 卡片
    ReasoningFoldToggled,
    /// 请求重绘
    RedrawRequested,
    /// 系统消息已追加到 UI mirror（banner / slash command 输出 / 错误提示等）.
    ///
    /// S2-C Step 2: 与 legacy `chat_mirror.lock().push_system_message(text)` 双写 —
    /// reducer 把消息 push 到 `ui.conversation_lines` 作为 Redux 自有 UI 账本.
    /// **注意**: 真实可见的 TUI 仍由 `chat_mirror` 渲染，本 Action 仅供 Redux 路径
    /// 维护一致的 UI 状态镜像 + 测试断言；S2-D/E 切闸到 Redux 单源时再删除 legacy mirror.
    SystemMessageAdded { text: String },
    /// Pure 模式下用户提交内容的视觉 echo — reducer push 一条 ConversationLine::User
    /// 到 ui.conversation_lines。legacy 模式由 chat_mirror.push_user_message 承担，
    /// Pure 模式守卫跳过 mirror 写后用此 Action 让 reducer 单源接管 echo。
    UserMessageEchoed(String),
    /// 后台会话常驻状态行更新（v1b）。`summary` 为空表示无后台会话（隐藏该行）。
    /// 由 chat 主循环在轮询 registry 后按需 dispatch（仅在内容变化时），reducer
    /// 把它写入 `ui.sessions_status`，经 `build_ui_snapshot` 反映到 renderer。
    SessionsStatusUpdated { summary: String },
    /// P1 sessions strip entries. This stays separate from the aggregate
    /// `sessions_status` text so the renderer does not parse display strings.
    SessionsEntriesUpdated {
        entries: Vec<crate::chat::sessions::SwitcherEntry>,
    },
    /// P2 active line-oriented child session viewport snapshot. `None` clears
    /// the child viewport when focus returns to main or PTY handoff resumes.
    ActiveSessionViewUpdated {
        view: Option<crate::chat::sessions::ActiveSessionView>,
    },
    /// 记录一个进入终态（或退出时被中断）的后台会话摘要（v4）。由 chat 主循环
    /// 在 `poll_finished` surface 每个 finished session 时、以及退出时为仍 running
    /// 的 session 各 dispatch 一次。reducer 把摘要 upsert（去重 by id）进
    /// `session.background_sessions`，随下次 `SaveSession` 落盘，reload 后展示。
    /// **只记录摘要，绝不重建进程/sub-agent/PTY**。
    BackgroundSessionRecorded {
        summary: crate::chat::sessions::PersistedSessionSummary,
    },
    /// 输入路由目标变更（v1.1b）。由 chat 主循环在 `/attach` / `/detach` 时
    /// dispatch（它独占权威 `attached_follow`），reducer 写入 `ui.focus`，经快照
    /// 驱动提示符的颜色+字形目标指示。`None` 等价 `FocusTarget::Main`。
    SessionFocusChanged { focus: crate::chat::sessions::FocusTarget },
    /// Ctrl+G 打开 session switcher 弹层（v1.1b）。`entries` 为打开时的会话快照
    /// （来自 1s 轮询缓存）。reducer 写入 `ui.switcher = Some(..)`。
    SwitcherOpened {
        entries: Vec<crate::chat::sessions::SwitcherEntry>,
    },
    /// switcher 选中行移动（v1.1b）。`selected` 为新的高亮索引（已被 key 线程
    /// 钳制到有效范围）。reducer 更新 `ui.switcher` 的 selected。
    SwitcherMoved { selected: usize },
    /// 关闭 switcher 弹层（v1.1b）。reducer 写入 `ui.switcher = None`。
    SwitcherClosed,

    // ── 退出 ────────────────────────────────────────────────────
    /// 单击 Ctrl+C — 取消当前生成
    CancelRequested,
    /// 双击 Ctrl+C / Ctrl+D / SIGTERM — 优雅退出
    ShutdownRequested,
    /// 兜底强制退出
    ForceQuit,
}

impl Action {
    /// S2.5 T2.5-2: 取 Action 变体名作为 `'static str` 用于 Prometheus label.
    ///
    /// 与 reduce 大 match 对齐，所有变体单字符串，无分配。
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::KeyPressed(_) => "KeyPressed",
            Self::PasteReceived(_) => "PasteReceived",
            Self::TerminalResized { .. } => "TerminalResized",
            Self::InputSubmitted(_) => "InputSubmitted",
            Self::HistoryNavigated(_) => "HistoryNavigated",
            Self::InputCancelled => "InputCancelled",
            Self::SlashCommandIssued { .. } => "SlashCommandIssued",
            Self::ModeChanged(_) => "ModeChanged",
            Self::ModelChanged { .. } => "ModelChanged",
            Self::ProviderChanged { .. } => "ProviderChanged",
            Self::HistoryCleared => "HistoryCleared",
            Self::HistoryClearedWithNotice { .. } => "HistoryClearedWithNotice",
            Self::TurnStarted { .. } => "TurnStarted",
            Self::StartLLMTurn { .. } => "StartLLMTurn",
            Self::StreamChunkReceived { .. } => "StreamChunkReceived",
            Self::StreamCompleted { .. } => "StreamCompleted",
            Self::StreamFailed { .. } => "StreamFailed",
            Self::StreamCancelled { .. } => "StreamCancelled",
            Self::ToolStarted { .. } => "ToolStarted",
            Self::ToolFinished { .. } => "ToolFinished",
            Self::ToolProgress { .. } => "ToolProgress",
            Self::ToolApprovalRequested { .. } => "ToolApprovalRequested",
            Self::ToolApprovalReceived { .. } => "ToolApprovalReceived",
            Self::StreamRetryAttempt { .. } => "StreamRetryAttempt",
            Self::SessionLoaded(_) => "SessionLoaded",
            Self::SessionSaved { .. } => "SessionSaved",
            Self::SessionSwitched { .. } => "SessionSwitched",
            Self::RecordUserTurn(_) => "RecordUserTurn",
            Self::RecordAssistantTurn(_) => "RecordAssistantTurn",
            Self::RecordSystemMessage { .. } => "RecordSystemMessage",
            Self::SetLeadingSystemPrompt { .. } => "SetLeadingSystemPrompt",
            Self::HistoryCompacted { .. } => "HistoryCompacted",
            Self::ToolCardFoldToggled => "ToolCardFoldToggled",
            Self::ReasoningFoldToggled => "ReasoningFoldToggled",
            Self::RedrawRequested => "RedrawRequested",
            Self::SystemMessageAdded { .. } => "SystemMessageAdded",
            Self::UserMessageEchoed(_) => "UserMessageEchoed",
            Self::SessionsStatusUpdated { .. } => "SessionsStatusUpdated",
            Self::SessionsEntriesUpdated { .. } => "SessionsEntriesUpdated",
            Self::ActiveSessionViewUpdated { .. } => "ActiveSessionViewUpdated",
            Self::BackgroundSessionRecorded { .. } => "BackgroundSessionRecorded",
            Self::SessionFocusChanged { .. } => "SessionFocusChanged",
            Self::SwitcherOpened { .. } => "SwitcherOpened",
            Self::SwitcherMoved { .. } => "SwitcherMoved",
            Self::SwitcherClosed => "SwitcherClosed",
            Self::CancelRequested => "CancelRequested",
            Self::ShutdownRequested => "ShutdownRequested",
            Self::ForceQuit => "ForceQuit",
        }
    }
}
