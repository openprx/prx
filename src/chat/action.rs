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
    /// 清除历史（/clear /new）
    HistoryCleared,

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

    // ── 退出 ────────────────────────────────────────────────────
    /// 单击 Ctrl+C — 取消当前生成
    CancelRequested,
    /// 双击 Ctrl+C / Ctrl+D / SIGTERM — 优雅退出
    ShutdownRequested,
    /// 兜底强制退出
    ForceQuit,
}
